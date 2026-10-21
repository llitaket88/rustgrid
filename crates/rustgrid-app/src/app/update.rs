//! Update detection and self-upgrade from GitHub Releases (see `.github/workflows/release.yml`).
//!
//! [`self_update`] (GitHub backend, ureq) drives the whole flow: list the repository's releases,
//! compare their semver against this build's `CARGO_PKG_VERSION`, download the portable archive for
//! the running target, extract the executable and replace the running one in place. Detection runs
//! once shortly after launch; a newer release surfaces an update button in the titlebar, and
//! 选项 → 关于 offers the same check plus a manual install.
//!
//! The asset names produced by the release workflow must contain the running platform tag (see
//! [`PLATFORM`]), otherwise `self_update` cannot pick the right download. The macOS archives carry an
//! app bundle rather than a bare executable, so installing one picks between `self_update`'s two
//! layouts — see [`configure_macos_layout`].

use super::*;
use std::io::Read as _;
use std::time::Duration;

/// The GitHub repository whose Releases are checked.
const REPO_OWNER: &str = "yisier";
const REPO_NAME: &str = "rustgrid";

/// The executable's base name. `self_update` appends the platform suffix (`.exe` on Windows) and
/// uses the result both to select the asset and to find the binary inside the archive.
const BIN_NAME: &str = "RustGrid";

/// The macOS app bundle inside the macOS archives (see `packaging/macos`).
#[cfg(target_os = "macos")]
const APP_BUNDLE: &str = "RustGrid.app";

/// This build's version, from the workspace `Cargo.toml`. It is compared against the release tag
/// (with its leading `v` stripped) to decide whether an update exists.
pub(super) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The platform tag this build downloads. It is a plain substring of the asset names minted by
/// `.github/workflows/release.yml` (e.g. `RustGrid-0.0.1-windows-x64.zip`); `self_update` matches
/// assets by substring, so any stable tag works — it need not be a Rust target triple.
pub(super) const PLATFORM: &str = if cfg!(target_os = "windows") {
    if cfg!(target_arch = "x86_64") {
        "windows-x64"
    } else {
        "windows-arm64"
    }
} else if cfg!(target_os = "macos") {
    if cfg!(target_arch = "x86_64") {
        "macos-x64"
    } else {
        "macos-arm64"
    }
} else if cfg!(target_arch = "x86_64") {
    "linux-x64"
} else {
    "linux-arm64"
};

/// macOS only: point the updater at the right member of the macOS archive.
///
/// macOS releases ship `RustGrid.app` (see `packaging/macos`), while the Windows/Linux archives hold
/// the bare executable at their root. Running from inside a bundle, the **whole bundle** is swapped
/// through `self_update`'s bundle mode, so the Info.plist and the icon are replaced together with
/// the binary instead of going stale against it. An install that predates the bundle — a bare
/// `RustGrid` file, i.e. anyone updating from an older release layout — has no `.app` to swap, so it
/// extracts the executable from inside the archive instead; the update still lands, the install just
/// stays bundle-less.
#[cfg(target_os = "macos")]
fn configure_macos_layout(builder: &mut self_update::backends::github::UpdateBuilder) {
    if running_from_app_bundle() {
        builder.bundle_path_in_archive(APP_BUNDLE);
    } else {
        builder.bin_path_in_archive(format!("{APP_BUNDLE}/Contents/MacOS/{BIN_NAME}"));
    }
}

/// Whether this executable lives inside a `.app` bundle. Bundle mode resolves its install path from
/// the nearest `.app` ancestor of `current_exe()`, so this is exactly the condition under which it
/// can be used.
#[cfg(target_os = "macos")]
fn running_from_app_bundle() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    exe.ancestors().any(|path| {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
    })
}

/// The state of the update check / install, driving the titlebar button and the 关于 page.
#[derive(Clone, Default)]
pub(super) enum UpdateStatus {
    /// No check has run yet.
    #[default]
    Idle,
    /// A check is in flight.
    Checking,
    /// The running version is the newest release.
    UpToDate,
    /// A newer release exists; its version is in `AppView::update_version`.
    Available,
    /// The newer release is downloading / being installed.
    Downloading,
    /// Installed; the process is about to relaunch.
    Installed,
    /// The check or install failed (the message is shown in 关于).
    Failed(String),
}

impl UpdateStatus {
    pub(super) fn is_available(&self) -> bool {
        matches!(self, UpdateStatus::Available)
    }

    /// Whether a check or install is running, so a second action must be ignored.
    pub(super) fn is_busy(&self) -> bool {
        matches!(self, UpdateStatus::Checking | UpdateStatus::Downloading)
    }
}

/// The repository's English changelog, for an English UI; the Chinese one is [`CHANGELOG_ZH_FILE`].
const CHANGELOG_EN_FILE: &str = "CHANGELOG.md";

/// The repository's Chinese changelog, for a 简体中文 UI (see `changelog`).
const CHANGELOG_ZH_FILE: &str = "CHANGELOG_zh.md";

/// Blocking: ask GitHub for the newest release and return its version and notes when it is strictly
/// newer than the running build. Runs on the tokio blocking pool via `Runtime::spawn_blocking`.
fn check_latest() -> Result<Option<(String, Option<String>)>, String> {
    let updater = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .target(PLATFORM)
        .current_version(APP_VERSION)
        .build()
        .map_err(|error| error.to_string())?;
    let release = updater
        .is_update_available()
        .map_err(|error| error.to_string())?;
    // The notes travel with the release (see `release_notes`), so 关于 can show what installing
    // would bring without a second check.
    Ok(release.map(|release| (release.version().to_string(), release_notes(&release))))
}

/// The newer release's notes as 关于 should show them — the release body when the release job wrote
/// it, and otherwise the version's section read out of the repository's changelogs.
///
/// The running build embeds the changelogs as of *its own* release, so it has no entry for a version
/// newer than itself; and every release published before the workflow copied the section into the
/// release body carries only GitHub's auto-generated `**Full Changelog**: …` line — which is not a
/// changelog. For those the note is fetched (one request per language, and only in that case, so a
/// release the job documented costs nothing extra), and 关于 then shows what installing would bring
/// instead of the running version's entry. A failed fetch falls back to the body as it is: a
/// hand-written release still says something worth reading.
fn release_notes(release: &self_update::Release) -> Option<String> {
    // A release with no description at all is no better than one whose description is not a
    // changelog, so it takes the same route; and with nothing to show the panel stays hidden (see
    // `options_about_body`) rather than reaching for the version this build already is.
    let body = release
        .body()
        .filter(|body| !body.trim().is_empty())
        .map(str::to_string);
    if body.as_deref().is_some_and(changelog::is_release_notes) {
        return body;
    }
    let fetched = match (
        fetch_repo_file(CHANGELOG_EN_FILE),
        fetch_repo_file(CHANGELOG_ZH_FILE),
    ) {
        (Some(english), Some(chinese)) => {
            changelog::notes_body(release.version(), &english, &chinese)
        }
        _ => None,
    };
    fetched.or(body)
}

/// Blocking: a file's text from the repository through GitHub's contents API.
///
/// `Accept: application/vnd.github.raw` makes the endpoint answer with the file itself rather than
/// its base64 JSON envelope, and leaving `?ref` off resolves to the repository's default branch,
/// whose changelog documents every released version — including one released before the tag carried
/// a changelog at all. Only failures that leave the panel without notes return `None`.
pub(super) fn fetch_repo_file(file: &str) -> Option<String> {
    use self_update::http_client::{HttpClient, UreqClient, header};
    let url = format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/contents/{file}");
    // GitHub rejects a request without a User-Agent, and the media type must be asked for by name.
    let mut headers = self_update::http_client::HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        format!("{BIN_NAME}/{APP_VERSION}").parse().ok()?,
    );
    headers.insert(header::ACCEPT, "application/vnd.github.raw".parse().ok()?);
    let client = UreqClient::default();
    let response = client
        .get(&url, &headers, Some(Duration::from_secs(20)))
        .ok()?;
    let mut text = String::new();
    response.body().read_to_string(&mut text).ok()?;
    Some(text)
}

/// Blocking: download and install the newest release, returning the installed version. The caller
/// relaunches the process afterwards.
///
/// Every chunk the transport reads is forwarded over `progress` as
/// `(bytes_downloaded_so_far, total_bytes)` so the UI can show a live progress bar; `total_bytes`
/// is `None` when the server sends no `Content-Length` (the bar then runs indeterminate). The
/// callback is `Fn` and the sender is `Send + Sync`, so it can be moved into `self_update`.
fn install_latest(
    progress: tokio::sync::mpsc::UnboundedSender<(u64, Option<u64>)>,
) -> Result<String, String> {
    let mut builder = self_update::backends::github::Update::configure();
    builder
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .target(PLATFORM)
        .current_version(APP_VERSION)
        // Non-interactive: never print to stdio or block on stdin (this is a GUI process).
        .no_confirm(true)
        .show_output(false)
        .show_download_progress(false)
        .progress_callback(move |downloaded, total| {
            // A closed receiver (the About window / app is gone) just drops the sample.
            let _ = progress.send((downloaded, total));
        });
    // Only the install path cares about the archive's shape: detection just matches the asset name.
    #[cfg(target_os = "macos")]
    configure_macos_layout(&mut builder);

    let updater = builder.build().map_err(|error| error.to_string())?;
    let status = updater.update().map_err(|error| error.to_string())?;
    Ok(status.version().to_string())
}

/// Human-readable byte count for the download progress caption (e.g. `12.3 MB`).
pub(super) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The download progress as a 0–100 percentage, when the server sent a positive `Content-Length`.
pub(super) fn progress_percent(downloaded: u64, total: Option<u64>) -> Option<u32> {
    match total {
        Some(total) if total > 0 => {
            Some(((downloaded.min(total) as f64 / total as f64) * 100.0).round() as u32)
        }
        _ => None,
    }
}

/// Relaunch the freshly replaced executable and exit this process. The child is marked with
/// `RUSTGRID_RESTARTED`, which makes the single-instance guard wait (see `single_instance`) for this
/// process to exit and release its lock before it decides another copy is running.
fn relaunch() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .env("RUSTGRID_RESTARTED", "1")
            .spawn();
    }
    std::process::exit(0);
}

impl AppView {
    /// Start a background update check. Deliberately does not notify: the startup call runs during
    /// `render` (where a frame is already in progress) and the completion callback notifies.
    ///
    /// `manual` (the 关于 button) reports failures in the UI; the quiet startup check swallows them
    /// (a missing network or a repository with no releases yet must not raise a red flag on launch).
    pub(super) fn check_for_updates(&mut self, manual: bool, cx: &mut Context<'_, Self>) {
        if self.update_status.is_busy() {
            return;
        }
        self.update_status = UpdateStatus::Checking;
        let runtime = self.runtime.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime.spawn_blocking(check_latest).await;
            let _ = this.update(cx, move |app, cx| {
                match result {
                    Ok(Ok(Some((version, notes)))) => {
                        app.update_version = Some(version);
                        app.update_notes = notes;
                        app.update_status = UpdateStatus::Available;
                    }
                    Ok(Ok(None)) => {
                        app.update_version = None;
                        app.update_notes = None;
                        app.update_status = UpdateStatus::UpToDate;
                    }
                    Ok(Err(error)) => {
                        app.fail_update_check(manual, error);
                    }
                    Err(error) => {
                        app.fail_update_check(manual, error.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Record a failed check: a manual check keeps the error for 关于; a quiet startup check resets
    /// to idle instead.
    fn fail_update_check(&mut self, manual: bool, error: String) {
        self.update_status = if manual {
            UpdateStatus::Failed(error)
        } else {
            UpdateStatus::Idle
        };
    }

    /// Download and install the newest release, then relaunch into it. Ignored while busy.
    ///
    /// The blocking install reports each read chunk over an unbounded channel; a second task
    /// drains it into `update_progress` so the titlebar and the 关于 page reflect the download
    /// live. The channel closes when the install finishes, which also ends the drain task.
    pub(super) fn install_update(&mut self, cx: &mut Context<'_, Self>) {
        if self.update_status.is_busy() || matches!(self.update_status, UpdateStatus::Installed) {
            return;
        }
        self.update_status = UpdateStatus::Downloading;
        self.update_progress = None;
        cx.notify();
        let runtime = self.runtime.clone();
        let (progress_tx, mut progress_rx) =
            tokio::sync::mpsc::unbounded_channel::<(u64, Option<u64>)>();

        // Drain download progress into the UI while the install runs on the blocking pool.
        cx.spawn(async move |this, cx| {
            while let Some((downloaded, total)) = progress_rx.recv().await {
                let _ = this.update(cx, |app, cx| {
                    app.update_progress = Some((downloaded, total));
                    cx.notify();
                });
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn_blocking(move || install_latest(progress_tx))
                .await;
            match result {
                Ok(Ok(version)) => {
                    let _ = this.update(cx, move |app, cx| {
                        app.update_version = Some(version);
                        app.update_status = UpdateStatus::Installed;
                        app.update_progress = None;
                        cx.notify();
                    });
                    // The on-disk executable has been replaced; hand over to it.
                    relaunch();
                }
                Ok(Err(error)) => {
                    let _ = this.update(cx, move |app, cx| {
                        app.update_status = UpdateStatus::Failed(error);
                        app.update_progress = None;
                        cx.notify();
                    });
                }
                Err(error) => {
                    let message = error.to_string();
                    let _ = this.update(cx, move |app, cx| {
                        app.update_status = UpdateStatus::Failed(message);
                        app.update_progress = None;
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// The human-readable status shown in 选项 → 关于.
    fn update_status_text(&self) -> String {
        match &self.update_status {
            UpdateStatus::Idle => String::new(),
            UpdateStatus::Checking => t!("about.checking").to_string(),
            UpdateStatus::UpToDate => t!("about.up_to_date").to_string(),
            UpdateStatus::Available => t!(
                "about.available",
                version = self.update_version.clone().unwrap_or_default()
            )
            .to_string(),
            UpdateStatus::Downloading => t!("about.downloading").to_string(),
            UpdateStatus::Installed => t!("about.installed").to_string(),
            UpdateStatus::Failed(error) => t!("about.failed", error = error.clone()).to_string(),
        }
    }

    /// The titlebar's update affordance, shown only when a check found a newer release (or an
    /// install is running / just finished). Returns an empty element when there is nothing to show.
    pub(super) fn titlebar_update_button(
        &self,
        theme: Theme,
        width: f32,
        cx: &mut Context<'_, Self>,
    ) -> AnyElement {
        // `label` is `None` for the available state: the update affordance is just a compact
        // download icon, with the version available on hover.
        let (icon, label, color) = match &self.update_status {
            UpdateStatus::Available => ("icons/update.svg", None, theme.brand),
            UpdateStatus::Downloading => {
                let label = match self
                    .update_progress
                    .and_then(|(downloaded, total)| progress_percent(downloaded, total))
                {
                    Some(percent) => {
                        t!("update.downloading_percent", percent = percent).to_string()
                    }
                    None => t!("update.downloading").to_string(),
                };
                ("icons/refresh.svg", Some(label), theme.text_muted)
            }
            UpdateStatus::Installed => (
                "icons/check.svg",
                Some(t!("update.restarting").to_string()),
                theme.brand,
            ),
            UpdateStatus::Failed(_) => (
                "icons/warning_mark.svg",
                Some(t!("update.retry").to_string()),
                theme.danger,
            ),
            UpdateStatus::Idle | UpdateStatus::Checking | UpdateStatus::UpToDate => {
                return div().into_any_element();
            }
        };
        let busy = matches!(
            self.update_status,
            UpdateStatus::Downloading | UpdateStatus::Installed
        );
        let tooltip = match &self.update_version {
            Some(version) => t!("about.available", version = version.clone()).to_string(),
            None => t!("update.button").to_string(),
        };
        let mut button = div()
            .id("titlebar-update")
            .flex()
            .flex_row()
            .occlude()
            .items_center()
            .justify_center()
            .gap_1()
            .min_w(px(width))
            // .h(px(26.0))
            // .rounded_full()
            .when(label.is_some(), |style| style.px_2())
            .when(label.is_none(), |style| style.w(px(26.0)))
            .text_size(px(12.0))
            .text_color(rgb(color))
            .when(!busy, move |style| {
                style
                    .cursor_pointer()
                    .hover(move |style| style.bg(rgb(theme.button_hover_bg)))
            })
            .on_click(cx.listener(|this, _event, _window, cx| {
                if this.update_status.is_available() {
                    this.install_update(cx);
                } else if matches!(this.update_status, UpdateStatus::Failed(_)) {
                    this.check_for_updates(true, cx);
                    cx.notify();
                }
            }))
            .child(
                svg()
                    .path(icon)
                    .w(px(16.0))
                    .h(px(16.0))
                    .flex_none()
                    .text_color(rgb(color)),
            );
        if let Some(label) = label {
            button = button.child(label);
        }
        button.tooltip(ui::text_tooltip(tooltip)).into_any_element()
    }

    /// The 关于 page: app identity, current version, the manual update controls and the changelog.
    pub(super) fn options_about_body(&self, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = self.theme;
        let status_color = if matches!(self.update_status, UpdateStatus::Failed(_)) {
            theme.danger
        } else {
            theme.text_muted
        };

        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(self.dialog_button(
                "about-check",
                t!("about.check").to_string(),
                false,
                cx.listener(|this, _event, _window, cx| {
                    this.check_for_updates(true, cx);
                    cx.notify();
                }),
            ));
        if self.update_status.is_available() {
            row = row.child(self.dialog_button(
                "about-install",
                t!("about.install").to_string(),
                true,
                cx.listener(|this, _event, _window, cx| this.install_update(cx)),
            ));
        }

        div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            // The changelog panel takes the height the page's own controls leave, so the page
            // reads as a filled box instead of an empty one under the status line.
            .h_full()
            .min_h(px(0.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        img(ImageSource::Resource(Resource::Embedded("logo.png".into())))
                            .w(px(48.0))
                            .h(px(48.0))
                            .flex_none(),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(16.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(t!("app.title").to_string()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(rgb(theme.text_muted))
                                    .child(t!("about.version", version = APP_VERSION).to_string()),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(theme.text_muted))
                    .child(t!("about.description").to_string()),
            )
            .child(row)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(status_color))
                    .child(self.update_status_text()),
            )
            .when_some(
                self.download_progress_element(theme),
                |element, progress| element.child(progress),
            )
            // The newer release's changelog — and only while there is one to describe (see
            // `changelog_panel`).
            .when_some(self.changelog_panel(theme), |element, panel| {
                element.child(panel)
            })
    }

    /// The changelog at the foot of 关于: what installing the newer release would bring, in the UI's
    /// own language (so a 简体中文界面 shows the Chinese changelog).
    ///
    /// `None` — the section stays off the page — when there is nothing to describe: this build is
    /// the newest release, or the newer release documents itself in neither place (see
    /// [`changelog`]). It never falls back to this build's own entry, which would label the running
    /// version as the update.
    fn changelog_panel(&self, theme: Theme) -> Option<AnyElement> {
        let notes = changelog::notes(self.language, self.update_notes.as_deref());
        if notes.is_empty() {
            return None;
        }
        let mut lines = div().flex().flex_col().gap_2().w_full();
        for note in notes {
            lines = lines.child(match note {
                changelog::NoteLine::Version { version, date } => div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme.text))
                            .child(version),
                    )
                    .when_some(date, |row, date| {
                        row.child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(theme.text_muted))
                                .child(date),
                        )
                    })
                    .into_any_element(),
                changelog::NoteLine::Group(group) => div()
                    .mt_1()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme.brand))
                    .child(group)
                    .into_any_element(),
                changelog::NoteLine::Bullet(item) => div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .w_full()
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.0))
                            .text_color(rgb(theme.text_muted))
                            .child("•"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(12.0))
                            .text_color(rgb(theme.text))
                            .child(item),
                    )
                    .into_any_element(),
                changelog::NoteLine::Text(text) => div()
                    .text_size(px(12.0))
                    .text_color(rgb(theme.text))
                    .child(text)
                    .into_any_element(),
            });
        }
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .flex_1()
                .min_h(px(0.0))
                .w_full()
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(theme.text_muted))
                        .child(t!("about.changelog").to_string()),
                )
                .child(
                    div()
                        .id("about-changelog")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h(px(0.0))
                        .w_full()
                        .p_3()
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(rgb(theme.border))
                        .bg(rgb(theme.dialog_face))
                        .overflow_y_scroll()
                        .child(lines),
                )
                .into_any_element(),
        )
    }

    /// The live download progress bar and byte caption for the 关于 page. `None` unless an install
    /// is running; the bar is indeterminate until the server's `Content-Length` gives a percentage.
    fn download_progress_element(&self, theme: Theme) -> Option<AnyElement> {
        if !matches!(self.update_status, UpdateStatus::Downloading) {
            return None;
        }
        let (downloaded, total) = self.update_progress.unwrap_or((0, None));
        let percent = progress_percent(downloaded, total);
        let caption = match (total, percent) {
            (Some(total), Some(percent)) if total > 0 => t!(
                "about.progress",
                percent = percent,
                done = format_bytes(downloaded),
                total = format_bytes(total)
            )
            .to_string(),
            _ => t!("about.progress_unknown", done = format_bytes(downloaded)).to_string(),
        };
        let bar = gpui_kit::component::progress::Progress::new("about-update-progress")
            .with_size(gpui_kit::component::Size::XSmall)
            .color(rgb(theme.brand))
            .value(percent.map(|percent| percent as f32).unwrap_or(0.0))
            // No total yet: run the indeterminate sliding animation (the byte caption still
            // advances), so the UI never looks frozen while the archive streams in.
            .loading(percent.is_none());
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .w_full()
                .child(bar)
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme.text_muted))
                        .child(caption),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A release as GitHub hands one to `check_latest`: a version and its body.
    fn release(body: &str) -> self_update::Release {
        let mut builder = self_update::Release::builder();
        builder
            .name("v0.0.11")
            .version("0.0.11")
            .date("2026-10-15")
            .body(body);
        builder.build().expect("0.0.11 is a release")
    }

    #[test]
    fn a_release_the_job_documented_keeps_its_own_notes() {
        // The release job already built the body out of the changelog (see release.yml), so it must
        // be what 关于 shows — as published, and without the fallback's two requests.
        let body = changelog::notes_body(
            "0.0.11",
            "## [0.0.11] - 2026-10-15\n\n### Added\n\n- published\n",
            "## [0.0.11] - 2026-10-15\n\n### 新增\n\n- 已发布\n",
        )
        .expect("both languages document 0.0.11");
        assert_eq!(
            release_notes(&release(&body)).as_deref(),
            Some(body.as_str())
        );
    }

    #[test]
    #[ignore = "network: fetches the repository's changelogs from the GitHub API"]
    fn a_release_without_its_own_notes_is_documented_from_the_repository() {
        // The reported bug: a release published before the workflow copied the changelog into its
        // body carries only GitHub's auto-generated `**Full Changelog**: …` line, so 关于 showed
        // that — or the running version's entry — instead of what installing would bring. The
        // repository's changelog documents the version, so the fallback must supply it in both
        // languages. The body below is verbatim what the v0.0.11 release carries.
        let notes = release_notes(&release(
            "**Full Changelog**: https://github.com/yisier/rustgrid/compare/v0.0.10...v0.0.11",
        ))
        .expect("the repository changelogs document 0.0.11");
        for language in [LanguageSetting::En, LanguageSetting::ZhCn] {
            let section = changelog::release_body_section(&notes, language)
                .unwrap_or_else(|| panic!("{language:?}: the notes carry a section per language"));
            assert!(
                section.starts_with("## [0.0.11]"),
                "{language:?}: 关于 must show 0.0.11's entry, got {section}"
            );
        }
    }
}
