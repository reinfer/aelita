// This file is released under the same terms as Rust itself.

mod cache;

use crossbeam;
use db;
use hyper;
use hyper::Url;
use hyper::buffer::BufReader;
use hyper::header::{self, qitem, Accept, Headers};
use hyper::net::{HttpListener, NetworkListener, NetworkStream};
use hyper::server::{Request, Response};
use hyper::status::StatusCode;
use pipeline::{self, PipelineId};
use rest::{authorization, Authorization, Client, Mime};
use serde_json::{
    self,
    from_slice as json_from_slice,
};
use std;
use std::borrow::Cow;
use std::collections::HashSet;
use std::io::BufWriter;
use std::iter;
use std::sync::Mutex;
use std::sync::mpsc::{Sender, Receiver};
use ui::{self, comments, Pr};
use util::USER_AGENT;
use util::github_headers;
use vcs::Commit;
use vcs::git::ToShortString;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TeamId(pub u32);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Repo {
    pub owner: String,
    pub repo: String,
}

#[derive(Clone, Debug)]
pub struct RepoPipelines {
    pub pipeline_id: PipelineId,
    pub try_pipeline_id: Option<PipelineId>,
}

#[derive(Clone, Copy, Debug)]
pub enum PipelineType {
    Stage,
    Try,
}

pub trait ProjectsConfig: Send + Sync + 'static {
    fn pipelines_by_repo(&self, &Repo) -> Option<RepoPipelines>;
    fn repo_by_pipeline(&self, PipelineId) -> Option<(Repo, PipelineType)>;
}

pub struct Worker {
    listen: String,
    projects: Box<ProjectsConfig>,
    client: Client<Authorization<authorization::Token>>,
    user_ident: String,
    secret: String,
    cache: Mutex<cache::Cache>,
}

impl Worker {
    pub fn new(
        listen: String,
        host: String,
        token: String,
        user: String,
        secret: String,
        projects: Box<ProjectsConfig>,
        cache_builder: db::Builder,
    ) -> Worker {
        let user_ident = format!("@{}", user);
        Worker {
            listen: listen,
            projects: projects,
            user_ident: user_ident,
            client: Client::new(USER_AGENT.to_owned())
                .base(&host)
                .authorization(Authorization(authorization::Token{
                    token: token,
                })),
            secret: secret,
            cache: Mutex::new(
                cache::from_builder(&cache_builder).expect("to get a cache")
            ),
        }
    }
}

// JSON API structs
#[derive(Deserialize, Serialize)]
struct IssueCommentPullRequest {
    html_url: String,
}
#[derive(Deserialize, Serialize)]
struct IssueCommentIssue {
    number: u32,
    title: String,
    body: Option<String>,
    pull_request: Option<IssueCommentPullRequest>,
    state: String,
    user: UserDesc,
}
#[derive(Deserialize, Serialize)]
struct IssueCommentComment {
    user: UserDesc,
    body: String,
}
#[derive(Deserialize, Serialize)]
struct PostCommentComment {
    body: String,
}
#[derive(Deserialize, Serialize)]
struct CommentDesc {
    issue: IssueCommentIssue,
    comment: IssueCommentComment,
    repository: RepositoryDesc,
}
#[derive(Deserialize, Serialize)]
struct RepositoryDesc {
    name: String,
    owner: OwnerDesc,
}
#[derive(Deserialize, Serialize)]
struct UserDesc {
    login: String,
    // type is a reserved word.
    #[serde(rename="type")]
    user_type: String,
}
#[derive(Deserialize, Serialize)]
struct OwnerDesc {
    login: String,
    // type is a reserved word.
    #[serde(rename="type")]
    owner_type: String,
}
#[derive(Deserialize, Serialize)]
struct PingDesc {
    zen: String,
}
#[derive(Deserialize, Serialize)]
struct TeamDesc {
    slug: String,
    id: u32,
}
#[derive(Deserialize, Serialize)]
struct TeamRepoDesc {
    permissions: Option<TeamRepoPermissions>,
}
#[derive(Deserialize, Serialize)]
struct TeamRepoPermissions {
    admin: bool,
    push: bool,
    pull: bool,
}
#[derive(Deserialize, Serialize)]
struct PrBranchDesc {
    sha: String,
}
#[derive(Deserialize, Serialize)]
struct PrDesc {
    state: String,
    number: u32,
    head: PrBranchDesc,
    html_url: String,
    title: String,
}
#[derive(Deserialize, Serialize)]
struct PullRequestDesc {
    action: String,
    pull_request: PrDesc,
    repository: RepositoryDesc,
}
#[derive(Deserialize, Serialize)]
struct StatusDesc {
    state: String,
    target_url: Option<String>,
    description: String,
    context: String,
}
#[derive(Deserialize, Serialize)]
struct TeamAddDesc {
    repository: RepositoryDesc,
}

impl pipeline::Worker<ui::Event, ui::Message> for Worker {
    fn run(
        &self,
        recv_msg: Receiver<ui::Message>,
        mut send_event: Sender<ui::Event>
    ) {
        crossbeam::scope(|scope| {
            let s2 = &*self;
            let send_event_2 = send_event.clone();
            scope.spawn(move || {
                s2.run_webhook(send_event_2);
            });
            loop {
                s2.handle_message(
                    recv_msg.recv().expect("Pipeline went away"),
                    &mut send_event,
                );
            }
        })
    }
}

impl Worker {
    fn run_webhook(
        &self,
        send_event: Sender<ui::Event>,
    ) {
        let mut listener = HttpListener::new(&self.listen[..])
            .expect("webhook");
        while let Ok(mut stream) = listener.accept() {
            let addr = stream.peer_addr()
                .expect("webhook client address");
            let mut stream_clone = stream.clone();
            let mut buf_read = BufReader::new(
                &mut stream_clone as &mut NetworkStream
            );
            let mut buf_write = BufWriter::new(&mut stream);
            let req = match Request::new(&mut buf_read, addr) {
                Ok(req) => req,
                Err(e) => {
                    warn!("Invalid webhook HTTP: {:?}", e);
                    continue;
                }
            };
            let mut head = Headers::new();
            let res = Response::new(&mut buf_write, &mut head);
            self.handle_webhook(req, res, &send_event);
        }
    }

    fn handle_webhook(
        &self,
        mut req: Request,
        mut res: Response,
        send_event: &Sender<ui::Event>
    ) {
        let head = github_headers::parse(&mut req, self.secret.as_bytes());
        let (x_github_event, body) = match head {
            Some(head) => head,
            None => return,
        };
        match &x_github_event[..] {
            b"issue_comment" => {
                if let Ok(desc) = json_from_slice::<CommentDesc>(&body) {
                    *res.status_mut() = StatusCode::NoContent;
                    if let Err(e) = res.send(&[]) {
                        warn!(
                            "Failed to send response to Github comment: {:?}",
                            e,
                        );
                    }
                    if !desc.comment.body.contains(&self.user_ident) {
                        info!("Comment does not mention me; do nothing");
                    } else if desc.issue.state == "closed" {
                        info!("Comment is for closed issue; do nothing");
                    } else if let Some(_) = desc.issue.pull_request {
                        info!("Got pull request comment");
                        self.handle_pr_comment(send_event, desc);
                    } else {
                        info!("Got issue comment; do nothing");
                    }
                } else {
                    warn!("Got invalid comment");
                    *res.status_mut() = StatusCode::BadRequest;
                    if let Err(e) = res.send(&[]) {
                        warn!(
                            "Failed to send response to bad comment: {:?}",
                            e,
                        );
                    }
                }
            }
            b"pull_request" => {
                if let Ok(desc) = json_from_slice::<PullRequestDesc>(&body) {
                    info!(
                        "Got PR message for #{}: {}",
                        desc.pull_request.number,
                        desc.action,
                    );
                    *res.status_mut() = StatusCode::NoContent;
                    if let Err(e) = res.send(&[]) {
                        warn!("Failed to send response to Github PR: {:?}", e);
                    }
                    let repo = Repo{
                        owner: desc.repository.owner.login,
                        repo: desc.repository.name,
                    };
                    let pr = Pr::from(desc.pull_request.number.to_string());
                    let repo_pipelines =
                        match self.projects.pipelines_by_repo(&repo) {
                            Some(repo_pipelines) => repo_pipelines,
                            None => {
                                warn!(
                                    "Got bad repo {:?}",
                                    repo
                                );
                                return;
                            }
                        };
                    let commit = Commit::from(
                        desc.pull_request.head.sha
                    );
                    if let Some(pipeline_id) = repo_pipelines.try_pipeline_id {
                        self.handle_pr_update(
                            &desc.action[..],
                            send_event,
                            pipeline_id,
                            commit.clone(),
                            pr.clone(),
                            desc.pull_request.title.clone(),
                            desc.pull_request.html_url.clone(),
                        );
                    }
                    self.handle_pr_update(
                        &desc.action[..],
                        send_event,
                        repo_pipelines.pipeline_id,
                        commit,
                        pr,
                        desc.pull_request.title,
                        desc.pull_request.html_url,
                    );
                } else {
                    warn!("Got invalid PR message");
                    *res.status_mut() = StatusCode::BadRequest;
                    if let Err(e) = res.send(&[]) {
                        warn!("Failed to send response to bad PR: {:?}", e);
                    }
                }
            }
            b"ping" => {
                if let Ok(desc) = json_from_slice::<PingDesc>(&body) {
                    info!("Got Ping: {}", desc.zen);
                    *res.status_mut() = StatusCode::NoContent;
                } else {
                    warn!("Got invalid Ping");
                    *res.status_mut() = StatusCode::BadRequest;
                }
                if let Err(e) = res.send(&[]) {
                    warn!("Failed to send response to Github ping: {:?}", e);
                }
            }
            b"team_add" => {
                if let Ok(desc) = json_from_slice::<TeamAddDesc>(&body) {
                    info!("Got team add event");
                    *res.status_mut() = StatusCode::NoContent;
                    if let Err(e) = res.send(&[]) {
                        warn!(
                            "Failed to send response to Github team add: {:?}",
                            e,
                        );
                    }
                    let repo = Repo{
                        owner: desc.repository.owner.login,
                        repo: desc.repository.name,
                    };
                    let repo_pipelines =
                        match self.projects.pipelines_by_repo(&repo) {
                            Some(repo_pipelines) => repo_pipelines,
                            None => {
                                warn!("team add event for nonexistant repo");
                                return;
                            }
                        };
                    let mut cache = self.cache.lock().unwrap();
                    let teams = match self.get_all_teams_with_write(&repo) {
                        Ok(t) => t,
                        Err(e) => {
                            warn!("Failed to refresh teams: {:?}", e);
                            return;
                        }
                    };
                    cache.set_teams_with_write(
                        repo_pipelines.pipeline_id,
                        teams.iter().cloned(),
                    );
                } else {
                    warn!("Got invalid team add event");
                    *res.status_mut() = StatusCode::BadRequest;
                    if let Err(e) = res.send(&[]) {
                        warn!(
                            "Failed to send response to bad Github team: {:?}",
                            e,
                        );
                    }
                }
            }
            e => {
                *res.status_mut() = StatusCode::BadRequest;
                if let Err(e) = res.send(&[]) {
                    warn!(
                        "Failed to send response to Github unknown: {:?}",
                        e,
                    );
                }
                warn!(
                    "Got Unknown Event {}",
                    String::from_utf8_lossy(&e)
                );
            }
        }
    }

    fn handle_pr_update(
        &self,
        action: &str,
        send_event: &Sender<ui::Event>,
        pipeline_id: PipelineId,
        commit: Commit,
        pr: Pr,
        title: String,
        html_url: String,
    ) {
        let event = match action {
            "closed" => Some(ui::Event::Closed(
                pipeline_id,
                pr,
            )),
            "opened" | "reopened" => Some(ui::Event::Opened(
                pipeline_id,
                pr,
                commit,
                title,
                Url::parse(&html_url).unwrap(),
            )),
            "synchronize" | "edited" => Some(ui::Event::Changed(
                pipeline_id,
                pr,
                commit,
                title,
                Url::parse(&html_url).unwrap(),
            )),
            _ => None,
        };
        if let Some(event) = event {
            send_event.send(event).expect("Pipeline to be there");
        }
    }

    fn handle_pr_comment(
        &self,
        send_event: &Sender<ui::Event>,
        desc: CommentDesc,
    ) {
        let repo = Repo{
            owner: desc.repository.owner.login,
            repo: desc.repository.name,
        };
        let pr = Pr::from(desc.issue.number.to_string());
        let repo_pipelines = match self.projects.pipelines_by_repo(&repo) {
            Some(repo_pipelines) => repo_pipelines,
            None => {
                warn!(
                    "Got bad repo {:?}",
                    repo
                );
                return;
            }
        };
        let user = &desc.comment.user.login;
        let body = &desc.comment.body;
        let pipeline_id = repo_pipelines.pipeline_id;
        let allowed = self.user_has_write(user, &repo, pipeline_id)
            .unwrap_or_else(|e| {
                warn!("Failed to check if {} has permission: {:?}", user, e);
                false
            });
        if !allowed {
            info!("Got mentioned by not-permitted user");
        } else if let Some(command) = comments::parse(&body, user) {
            self.handle_comment_command(
                send_event,
                command,
                &desc.issue,
                &repo_pipelines,
                pr,
            );
        } else {
            info!("Pull request comment is not a command");
        }
    }

    fn handle_comment_command(
        &self,
        send_event: &Sender<ui::Event>,
        command: comments::Command,
        issue: &IssueCommentIssue,
        repo_pipelines: &RepoPipelines,
        pr: Pr,
    ) {
        match command {
            comments::Command::Approved(user, commit) => {
                self.handle_approved_pr(
                    repo_pipelines.pipeline_id,
                    send_event,
                    issue,
                    pr,
                    user,
                    commit,
                );
            }
            comments::Command::Canceled => {
                self.handle_canceled_pr(
                    repo_pipelines.pipeline_id,
                    send_event,
                    pr,
                );
            }
            comments::Command::TryApproved(user, commit) => {
                if let Some(try_pipeline_id) = repo_pipelines.try_pipeline_id {
                    self.handle_approved_pr(
                        try_pipeline_id,
                        send_event,
                        issue,
                        pr,
                        user,
                        commit,
                    );
                }
            }
            comments::Command::TryCanceled => {
                if let Some(try_pipeline_id) = repo_pipelines.try_pipeline_id {
                    self.handle_canceled_pr(
                        try_pipeline_id,
                        send_event,
                        pr,
                    );
                }
            }
        }
    }

    fn handle_approved_pr(
        &self,
        pipeline_id: PipelineId,
        send_event: &Sender<ui::Event>,
        issue: &IssueCommentIssue,
        pr: Pr,
        user: &str,
        commit: Option<Commit>,
    ) {
        let message = format!(
            "{}\n\nMerge #{} a=@{} r=@{}\n{}\n\n{}",
            issue.title,
            pr,
            issue.user.login,
            user,
            iter::repeat('_').take(72).collect::<String>(),
            issue.body.as_ref().map(|x| &x[..]).unwrap_or(""),
        );
        send_event.send(ui::Event::Approved(
            pipeline_id,
            pr,
            commit,
            message,
        )).expect("PR Approved: Pipeline error");
    }

    fn handle_canceled_pr(
        &self,
        pipeline_id: PipelineId,
        send_event: &Sender<ui::Event>,
        pr: Pr,
    ) {
        send_event.send(ui::Event::Canceled(
            pipeline_id,
            pr,
        )).expect("PR Canceled: Pipeline error");
    }

    fn handle_message(
        &self,
        msg: ui::Message,
        _: &mut Sender<ui::Event>,
    ) {
        match msg {
            ui::Message::SendResult(pipeline_id, pr, status) => {
                let result = self.send_result_to_pr(pipeline_id, &pr, &status);
                if let Err(e) = result {
                    warn!("Failed to send {:?} to pr {}: {:?}", status, pr, e)
                }
            }
        }
    }

    fn user_has_write(
        &self,
        user: &str,
        repo: &Repo,
        pipeline_id: PipelineId,
    ) -> Result<bool, GithubRequestError> {
        let mut cache = self.cache.lock().unwrap();
        match cache.is_org(pipeline_id) {
            Some(true) => {
                info!("Using teams permission check");
                let teams = cache.teams_with_write(pipeline_id);
                let mut allowed = false;
                for team in teams {
                    if try!(self.user_is_member_of(user, team)) {
                        allowed = true;
                        break;
                    }
                }
                Ok(allowed)
            }
            Some(false) => {
                info!("Using users permission check");
                self.user_is_collaborator_for(user, repo)
            }
            None => {
                info!("Loading teams");
                let is_org = try!(self.repo_is_org(repo));
                cache.set_is_org(pipeline_id, is_org);
                if is_org {
                    cache.set_teams_with_write(
                        pipeline_id,
                        try!(self.get_all_teams_with_write(repo))
                            .iter()
                            .cloned()
                    );
                }
                drop(cache);
                self.user_has_write(user, repo, pipeline_id)
            }
        }
    }

    fn send_result_to_pr(
        &self,
        pipeline_id: PipelineId,
        pr: &Pr,
        status: &ui::Status,
    ) -> Result<(), GithubRequestError> {
        let (repo, pipeline_type) =
            match self.projects.repo_by_pipeline(pipeline_id) {
                Some(result) => result,
                None => {
                    return Err(GithubRequestError::Pipeline(pipeline_id));
                }
            };
        let comment_body = match *status {
            ui::Status::Approved(_) => None,
            ui::Status::StartingBuild(_, _) => None,
            ui::Status::Testing(_, _, _) => None,
            ui::Status::Success(_, _, ref url) => Some({
                if let Some(ref url) = *url {
                    Cow::Owned(format!(":+1: [Build succeeded]({})", url))
                } else {
                    Cow::Borrowed(":+1: Build succeeded")
                }
            }),
            ui::Status::Failure(_, _, ref url) => Some({
                if let Some(ref url) = *url {
                    Cow::Owned(format!(":-1: [Build failed]({})", url))
                } else {
                    Cow::Borrowed(":-1: Build failed")
                }
            }),
            ui::Status::Unmergeable(_) => Some(Cow::Borrowed(
                ":x: Merge conflict!"
            )),
            ui::Status::Unmoveable(_, _) => Some(Cow::Borrowed(
                ":scream: Internal error while fast-forward master"
            )),
            ui::Status::Invalidated => Some(Cow::Borrowed(
                ":no_good: New commits added"
            )),
            ui::Status::NoCommit => Some(Cow::Borrowed(
                ":scream: Internal error: no commit found for PR"
            )),
            ui::Status::Completed(_, _) => None,
        };
        let context = match pipeline_type {
            PipelineType::Stage => "continuous-integration/aelita",
            PipelineType::Try => "continuous-integration/aelita/try",
        }.to_owned();
        let status = match *status {
            ui::Status::Approved(ref pull_commit) => Some((
                pull_commit,
                None,
                StatusDesc {
                    state: "pending".to_owned(),
                    target_url: None,
                    description: format!(
                        "Approved {}",
                        pull_commit,
                    ),
                    context: context,
                }
            )),
            ui::Status::StartingBuild(
                ref pull_commit,
                ref merge_commit,
            ) => Some((
                pull_commit,
                Some(merge_commit),
                StatusDesc {
                    state: "pending".to_owned(),
                    target_url: None,
                    description: format!(
                        "Testing {} with merge commit {}",
                        &pull_commit.to_short_string()[..],
                        &merge_commit.to_short_string()[..],
                    ),
                    context: context,
                }
            )),
            ui::Status::Testing(
                ref pull_commit,
                ref merge_commit,
                ref url,
            ) => Some((
                pull_commit,
                Some(merge_commit),
                StatusDesc {
                    state: "pending".to_owned(),
                    target_url: url.as_ref().map(ToString::to_string),
                    description: format!(
                        "Testing {} with merge commit {}",
                        &pull_commit.to_short_string()[..],
                        &merge_commit.to_short_string()[..],
                    ),
                    context: context,
                }
            )),
            ui::Status::Success(
                ref pull_commit,
                ref merge_commit,
                ref url,
            ) => Some((
                pull_commit,
                Some(merge_commit),
                StatusDesc {
                    state: "success".to_owned(),
                    target_url: url.as_ref().map(ToString::to_string),
                    description: "Tests passed".to_owned(),
                    context: context,
                }
            )),
            ui::Status::Failure(
                ref pull_commit,
                ref merge_commit, 
                ref url,
            ) => Some((
                pull_commit,
                Some(merge_commit),
                StatusDesc {
                    state: "failure".to_owned(),
                    target_url: url.as_ref().map(ToString::to_string),
                    description: "Tests failed".to_owned(),
                    context: context,
                }
            )),
            ui::Status::Unmergeable(
                ref pull_commit,
            ) => Some((
                pull_commit,
                None,
                StatusDesc {
                    state: "failure".to_owned(),
                    target_url: None,
                    description: "Merge failed".to_owned(),
                    context: context,
                }
            )),
            ui::Status::Unmoveable(
                ref pull_commit,
                ref merge_commit,
            ) => Some((
                pull_commit,
                Some(merge_commit),
                StatusDesc {
                    state: "error".to_owned(),
                    target_url: None,
                    description: "Merge failed".to_owned(),
                    context: context,
                }
            )),
            ui::Status::Invalidated | ui::Status::NoCommit => None,
            ui::Status::Completed(_, _) => None,
        };
        if let Some(comment_body) = comment_body {
            let url = format!(
                "/repos/{}/{}/issues/{}/comments",
                repo.owner,
                repo.repo,
                pr
            );
            let comment = PostCommentComment{
                body: comment_body.into_owned(),
            };
            let resp = try!(
                try!(self.client.post(&url).expect("url").json(&comment))
                    .header(Self::accept(AcceptType::Regular))
                    .send()
            );
            if !resp.is_success() {
                return Err(GithubRequestError::HttpStatus(resp.http.status))
            }
        }
        if let Some(status) = status {
            let (pull_commit, merge_commit, status_body) = status;
            let url = format!(
                "/repos/{}/{}/statuses/{}",
                repo.owner,
                repo.repo,
                pull_commit
            );
            let resp = try!(
                try!(self.client.post(&url).expect("url").json(&status_body))
                    .header(Self::accept(AcceptType::Regular))
                    .send()
            );
            if !resp.is_success() {
                return Err(GithubRequestError::HttpStatus(resp.http.status))
            }
            if let Some(merge_commit) = merge_commit {
                let url = format!(
                    "/repos/{}/{}/statuses/{}",
                    repo.owner,
                    repo.repo,
                    merge_commit
                );
                let resp = try!(
                    try!(
                        self.client.post(&url).expect("url")
                            .json(&status_body)
                    )
                        .header(Self::accept(AcceptType::Regular))
                        .send()
                );
                if !resp.is_success() {
                    return Err(GithubRequestError::HttpStatus(
                    	resp.http.status
                    ))
                }
            }
        }
        Ok(())
    }

    fn user_is_member_of(
        &self,
        user: &str,
        team: TeamId,
    ) -> Result<bool, GithubRequestError> {
        let url = format!(
            "/teams/{}/members/{}",
            team.0,
            user,
        );
        let resp = try!(
            self.client.get(&url).expect("valid url")
                .header(Self::accept(AcceptType::Regular))
                .send()
        );
        if resp.http.status == StatusCode::NotFound {
            Ok(false)
        } else if resp.is_success() {
            Ok(true)
        } else {
            Err(GithubRequestError::HttpStatus(resp.http.status))
        }
    }

    fn user_is_collaborator_for(
        &self,
        user: &str,
        repo: &Repo,
    ) -> Result<bool, GithubRequestError> {
        let url = format!(
            "/repos/{}/{}/collaborators/{}",
            repo.owner,
            repo.repo,
            user,
        );
        let resp = try!(
            self.client.get(&url).expect("valid url")
                .header(Self::accept(AcceptType::Regular))
                .send()
        );
        if resp.http.status == StatusCode::NotFound {
            Ok(false)
        } else if resp.is_success() {
            Ok(true)
        } else {
            Err(GithubRequestError::HttpStatus(resp.http.status))
        }
    }

    fn repo_is_org(
        &self,
        repo: &Repo,
    ) -> Result<bool, GithubRequestError> {
        let url = format!(
            "/repos/{}/{}",
            repo.owner,
            repo.repo,
        );
        let resp = try!(
            self.client.get(&url).expect("valid url")
                .header(Self::accept(AcceptType::Regular))
                .send()
        );
        if resp.is_success() {
            let repo_desc: RepositoryDesc = try!(resp.json());
            Ok(match &repo_desc.owner.owner_type[..] {
                "User" => false,
                "Organization" => true,
                _ => {
                    warn!(
                        "Unknown owner type: {}",
                        repo_desc.owner.owner_type,
                    );
                    false
                }
            })
        } else {
            Err(GithubRequestError::HttpStatus(resp.http.status))
        }
    }

    fn get_all_teams_with_write(
        &self,
        repo: &Repo,
    ) -> Result<HashSet<TeamId>, GithubRequestError> {
        let url = format!(
            "/orgs/{}/teams",
            repo.owner,
        );
        let resp = try!(
            self.client.get(&url).expect("valid url")
                .header(Self::accept(AcceptType::Regular))
                .send()
        );
        if resp.is_success() {
            let all_teams: Vec<TeamDesc> = try!(resp.json());
            let mut writing_teams = HashSet::new();
            for team in all_teams {
                let url = format!(
                    "/teams/{}/repos/{}/{}",
                    team.id,
                    repo.owner,
                    repo.repo
                );
                let resp = try!(
                    self.client.get(&url).expect("valid url")
                        .header(Self::accept(AcceptType::Repository))
                        .send()
                );
                let team_repo: TeamRepoDesc = try!(resp.json());
                if let Some(ref permissions) = team_repo.permissions {
                    if permissions.admin || permissions.push {
                        writing_teams.insert(TeamId(team.id));
                    }
                }
            }
            Ok(writing_teams)
        } else {
            Err(GithubRequestError::HttpStatus(resp.http.status))
        }
    }
    fn accept(accept_type: AcceptType) -> Accept {
        let mime: Mime = match accept_type {
            AcceptType::Regular => "application/vnd.github.v3+json",
            AcceptType::Repository =>
                "application/vnd.github.v3.repository+json",
        }.parse().expect("hard-coded mimes to be valid");
        header::Accept(vec![qitem(mime)])
    }
}

enum AcceptType {
    Regular,
    Repository,
}

quick_error! {
    #[derive(Debug)]
    pub enum GithubRequestError {
        /// HTTP-level error
        HttpStatus(status: StatusCode) {}
        /// HTTP-level error
        Http(err: hyper::error::Error) {
            cause(err)
            from()
        }
        /// Integer parsing error
        Int(err: std::num::ParseIntError) {
            cause(err)
            from()
        }
        /// JSON error
        Json(err: serde_json::error::Error) {
            cause(err)
            from()
        }
        /// Repo not found for pipeline
        Pipeline(pipeline_id: PipelineId) {}
    }
}
