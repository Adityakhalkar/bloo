use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow};
use gpui::{
    Animation, AnimationExt as _, App, AsyncApp, Context, ElementId, Empty, Entity, Subscription,
    Task, WeakEntity, Window, pulsating_between,
};
use project::{
    Project,
    git_store::{GitStoreEvent, RepositoryEvent},
};
use serde::Deserialize;
use ui::{ButtonLike, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

const IN_PROGRESS_POLL_INTERVAL: Duration = Duration::from_secs(15);
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// The deployments endpoint ignores a branch filter, so fetch enough to find the current branch.
const DEPLOYMENTS_TO_SCAN: usize = 20;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let status = cx.new(|cx| VercelStatus::new(workspace, cx));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_left_item(status, window, cx);
        });
    })
    .detach();
}

/// Shows the latest Vercel deployment for the active repository's branch.
/// Only appears in repositories linked with `vercel link` (they have `.vercel/project.json`).
pub struct VercelStatus {
    project: Entity<Project>,
    state: Option<DisplayState>,
    poll_task: Task<()>,
    _subscription: Subscription,
}

enum DisplayState {
    Deployment {
        deployment: Deployment,
        current_branch: SharedString,
    },
    Error(SharedString),
}

#[derive(Deserialize)]
struct ProjectLink {
    #[serde(rename = "projectId")]
    project_id: String,
    #[serde(rename = "orgId")]
    org_id: String,
}

#[derive(Deserialize)]
struct DeploymentList {
    deployments: Vec<Deployment>,
}

#[derive(Clone, Debug, Deserialize)]
struct Deployment {
    url: String,
    state: DeploymentState,
    #[serde(rename = "inspectorUrl")]
    inspector_url: Option<String>,
    /// Unix time in milliseconds.
    created: u64,
    #[serde(default)]
    meta: DeploymentMeta,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct DeploymentMeta {
    #[serde(rename = "githubCommitRef")]
    github_commit_ref: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum DeploymentState {
    Queued,
    Initializing,
    Building,
    Ready,
    Error,
    Canceled,
    #[serde(other)]
    Unknown,
}

impl DeploymentState {
    fn is_in_progress(self) -> bool {
        matches!(self, Self::Queued | Self::Initializing | Self::Building)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Initializing | Self::Building => "Building",
            Self::Ready => "Ready",
            Self::Error => "Failed",
            Self::Canceled => "Canceled",
            Self::Unknown => "Unknown",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Ready => Color::Success,
            Self::Error => Color::Error,
            Self::Queued | Self::Initializing | Self::Building => Color::Warning,
            Self::Canceled | Self::Unknown => Color::Muted,
        }
    }
}

impl VercelStatus {
    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let subscription = cx.subscribe(&git_store, |this, _, event, cx| match event {
            GitStoreEvent::ActiveRepositoryChanged(_)
            | GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::HeadChanged, true) => {
                this.restart_polling(cx);
            }
            _ => {}
        });
        let mut this = Self {
            project,
            state: None,
            poll_task: Task::ready(()),
            _subscription: subscription,
        };
        this.restart_polling(cx);
        this
    }

    fn restart_polling(&mut self, cx: &mut Context<Self>) {
        let target = {
            let project = self.project.read(cx);
            if project.is_local() {
                project.active_repository(cx).and_then(|repository| {
                    let repository = repository.read(cx);
                    let branch = repository.branch.as_ref()?.name().to_string();
                    Some((repository.work_directory_abs_path.to_path_buf(), branch))
                })
            } else {
                None
            }
        };

        let Some((root, branch)) = target else {
            self.state = None;
            self.poll_task = Task::ready(());
            cx.notify();
            return;
        };

        self.poll_task = cx.spawn(async move |this, cx| {
            Self::poll(this, root, branch.into(), cx).await.log_err();
        });
    }

    async fn poll(
        this: WeakEntity<Self>,
        root: PathBuf,
        branch: SharedString,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let link_path = root.join(".vercel").join("project.json");
        let Ok(link) = smol::fs::read_to_string(&link_path).await else {
            this.update(cx, |this, cx| {
                this.state = None;
                cx.notify();
            })?;
            return Ok(());
        };
        let link: ProjectLink = serde_json::from_str(&link)
            .with_context(|| format!("parsing {}", link_path.display()))?;

        loop {
            let state = match fetch_deployments(&root, &link).await {
                Ok(deployments) => {
                    latest_for_branch(deployments, &branch).map(|deployment| {
                        DisplayState::Deployment {
                            deployment,
                            current_branch: branch.clone(),
                        }
                    })
                }
                Err(error) => Some(DisplayState::Error(format!("{error:#}").into())),
            };
            let interval = match &state {
                Some(DisplayState::Deployment { deployment, .. })
                    if deployment.state.is_in_progress() =>
                {
                    IN_PROGRESS_POLL_INTERVAL
                }
                _ => IDLE_POLL_INTERVAL,
            };
            this.update(cx, |this, cx| {
                this.state = state;
                cx.notify();
            })?;
            cx.background_executor().timer(interval).await;
        }
    }
}

async fn fetch_deployments(root: &Path, link: &ProjectLink) -> Result<Vec<Deployment>> {
    let endpoint = format!(
        "/v6/deployments?projectId={}&limit={DEPLOYMENTS_TO_SCAN}",
        link.project_id
    );
    match run_vercel_api(root, &endpoint).await {
        Ok(deployments) => Ok(deployments),
        // The CLI applies its current scope; a project owned by another team needs `teamId`.
        Err(_) => run_vercel_api(root, &format!("{endpoint}&teamId={}", link.org_id)).await,
    }
}

async fn run_vercel_api(root: &Path, endpoint: &str) -> Result<Vec<Deployment>> {
    let mut command = util::command::new_command("vercel");
    command.args(["api", endpoint, "--raw"]).current_dir(root);
    let output = command
        .output()
        .await
        .context("couldn't run the Vercel CLI. Install it with `npm i -g vercel`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "{}. If you're signed out, run `vercel login`",
            stderr.trim()
        ));
    }
    let list: DeploymentList =
        serde_json::from_slice(&output.stdout).context("unexpected Vercel API response")?;
    Ok(list.deployments)
}

/// Deployments arrive newest first. Prefer the newest one built from `branch`;
/// otherwise fall back to the newest deployment of any branch.
fn latest_for_branch(deployments: Vec<Deployment>, branch: &str) -> Option<Deployment> {
    let matching = deployments
        .iter()
        .position(|deployment| deployment.meta.github_commit_ref.as_deref() == Some(branch));
    let index = matching.unwrap_or(0);
    deployments.into_iter().nth(index)
}

pub(crate) fn format_age(created_ms: u64, now_ms: u64) -> String {
    let minutes = now_ms.saturating_sub(created_ms) / 60_000;
    match minutes {
        0 => "just now".to_string(),
        1..=59 => format!("{minutes}m ago"),
        60..=1439 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl Render for VercelStatus {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.state {
            None => Empty.into_any_element(),
            Some(DisplayState::Error(message)) => {
                let message = message.clone();
                ButtonLike::new("vercel-status")
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Label::new("▲").size(LabelSize::Small).color(Color::Muted))
                            .child(
                                Label::new("Vercel unavailable")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .tooltip(move |_, cx| {
                        Tooltip::with_meta(
                            "Couldn't load Vercel deployments",
                            None,
                            message.clone(),
                            cx,
                        )
                    })
                    .into_any_element()
            }
            Some(DisplayState::Deployment {
                deployment,
                current_branch,
            }) => {
                let deployment_branch = deployment.meta.github_commit_ref.clone();
                let is_current_branch =
                    deployment_branch.as_deref() == Some(current_branch.as_ref());
                let open_url = if deployment.state == DeploymentState::Ready {
                    format!("https://{}", deployment.url)
                } else {
                    deployment
                        .inspector_url
                        .clone()
                        .unwrap_or_else(|| format!("https://{}", deployment.url))
                };
                let title: SharedString = format!(
                    "Vercel · {} · {}",
                    deployment.state.label(),
                    deployment_branch.as_deref().unwrap_or("unknown branch")
                )
                .into();
                let age = format_age(deployment.created, now_ms());
                let meta: SharedString = if is_current_branch {
                    format!("{age} · {} · Click to open", deployment.url).into()
                } else {
                    format!("No deploy for {current_branch} yet. Showing the latest, {age}. Click to open")
                        .into()
                };
                let mut label = deployment.state.label().to_string();
                if !is_current_branch && let Some(branch) = &deployment_branch {
                    label.push_str(&format!(" · {branch}"));
                }

                let triangle = Label::new("▲")
                    .size(LabelSize::Small)
                    .color(deployment.state.color());
                let triangle = if deployment.state.is_in_progress() {
                    // A slow breath while Vercel is still building.
                    triangle
                        .with_animation(
                            "vercel-building",
                            Animation::new(Duration::from_millis(1400))
                                .repeat()
                                .with_easing(pulsating_between(0.35, 1.0)),
                            |label, delta| label.alpha(delta),
                        )
                        .into_any_element()
                } else {
                    triangle.into_any_element()
                };
                // A new deployment or state change rises in, so the change is noticed.
                let change_id = ElementId::NamedInteger(
                    SharedString::from(format!("vercel-state-{}", deployment.state.label())),
                    deployment.created,
                );

                ButtonLike::new("vercel-status")
                    .child(
                        h_flex()
                            .relative()
                            .gap_1()
                            .child(triangle)
                            .child(Label::new(label).size(LabelSize::Small))
                            .with_animation(
                                change_id,
                                ui::motion::enter_animation(),
                                |this, delta| {
                                    this.opacity(delta)
                                        .top(px(ui::motion::enter_offset(delta) / 2.0))
                                },
                            ),
                    )
                    .tooltip(move |_, cx| Tooltip::with_meta(title.clone(), None, meta.clone(), cx))
                    .on_click(move |_, _, cx| cx.open_url(&open_url))
                    .into_any_element()
            }
        }
    }
}

impl StatusItemView for VercelStatus {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    // Shown only in repositories linked to a Vercel project, so there's nothing to hide by default.
    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSE: &str = r#"{"deployments":[
        {"url":"app-1.vercel.app","state":"BUILDING","inspectorUrl":"https://vercel.com/i/1","created":3000,"meta":{"githubCommitRef":"feature"}},
        {"url":"app-2.vercel.app","state":"READY","inspectorUrl":"https://vercel.com/i/2","created":2000,"meta":{"githubCommitRef":"main"}},
        {"url":"app-3.vercel.app","state":"DELETED","created":1000}
    ],"pagination":{}}"#;

    #[test]
    fn picks_newest_deployment_for_branch() {
        let list: DeploymentList = serde_json::from_str(RESPONSE).unwrap();
        assert_eq!(list.deployments[2].state, DeploymentState::Unknown);

        let main = latest_for_branch(list.deployments.clone(), "main").unwrap();
        assert_eq!(main.url, "app-2.vercel.app");
        assert_eq!(main.state, DeploymentState::Ready);

        let missing = latest_for_branch(list.deployments, "no-such-branch").unwrap();
        assert_eq!(missing.url, "app-1.vercel.app");
        assert!(missing.state.is_in_progress());

        assert!(latest_for_branch(Vec::new(), "main").is_none());
    }

    #[test]
    fn formats_age() {
        assert_eq!(format_age(0, 30_000), "just now");
        assert_eq!(format_age(0, 5 * 60_000), "5m ago");
        assert_eq!(format_age(0, 3 * 3_600_000), "3h ago");
        assert_eq!(format_age(0, 2 * 86_400_000), "2d ago");
        assert_eq!(format_age(10, 0), "just now");
    }
}
