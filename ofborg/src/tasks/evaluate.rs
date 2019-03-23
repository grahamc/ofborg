/// This is what evaluates every pull-request
extern crate amqp;
extern crate env_logger;
extern crate uuid;

use amqp::protocol::basic::{BasicProperties, Deliver};
use hubcaps;
use hubcaps::issues::Issue;
use ofborg::acl::ACL;
use ofborg::checkout;
use ofborg::commitstatus::CommitStatus;
use ofborg::files::file_to_str;
use ofborg::message::{buildjob, evaluationjob};
use ofborg::nix;
use ofborg::stats;
use ofborg::stats::Event;
use ofborg::systems;
use ofborg::worker;
use std::collections::HashMap;
use std::path::Path;
use tasks::eval;

pub struct EvaluationWorker<E> {
    cloner: checkout::CachedCloner,
    nix: nix::Nix,
    github: hubcaps::Github,
    acl: ACL,
    identity: String,
    events: E,
    tag_paths: HashMap<String, Vec<String>>,
}

impl<E: stats::SysEvents> EvaluationWorker<E> {
    pub fn new(
        cloner: checkout::CachedCloner,
        nix: &nix::Nix,
        github: hubcaps::Github,
        acl: ACL,
        identity: String,
        events: E,
        tag_paths: HashMap<String, Vec<String>>,
    ) -> EvaluationWorker<E> {
        EvaluationWorker {
            cloner,
            nix: nix.without_limited_supported_systems(),
            github,
            acl,
            identity,
            events,
            tag_paths,
        }
    }

    fn actions(&self) -> evaluationjob::Actions {
        evaluationjob::Actions {}
    }
}

impl<E: stats::SysEvents + 'static> worker::SimpleWorker for EvaluationWorker<E> {
    type J = evaluationjob::EvaluationJob;

    fn msg_to_job(
        &mut self,
        _: &Deliver,
        _: &BasicProperties,
        body: &[u8],
    ) -> Result<Self::J, String> {
        self.events.notify(Event::JobReceived);
        match evaluationjob::from(body) {
            Ok(e) => {
                self.events.notify(Event::JobDecodeSuccess);
                Ok(e)
            }
            Err(e) => {
                self.events.notify(Event::JobDecodeFailure);
                error!(
                    "Failed to decode message: {:?}, Err: {:?}",
                    String::from_utf8(body.to_vec()),
                    e
                );
                Err("Failed to decode message".to_owned())
            }
        }
    }

    fn consumer(&mut self, job: &evaluationjob::EvaluationJob) -> worker::Actions {
        let repo = self
            .github
            .repo(job.repo.owner.clone(), job.repo.name.clone());
        let gists = self.github.gists();
        let pulls = repo.pulls();
        let pull = pulls.get(job.pr.number);
        let issue_ref = repo.issue(job.pr.number);
        let issue: Issue;
        let auto_schedule_build_archs: Vec<systems::System>;


        match issue_ref.get() {
            Ok(iss) => {
                if iss.state == "closed" {
                    self.events.notify(Event::IssueAlreadyClosed);
                    info!("Skipping {} because it is closed", job.pr.number);
                    return self.actions().skip(&job);
                }

                if issue_is_wip(&iss) {
                    auto_schedule_build_archs = vec![];
                } else {
                    auto_schedule_build_archs = self.acl.build_job_architectures_for_user_repo(
                        &iss.user.login,
                        &job.repo.full_name,
                    );
                }

                issue = iss;
            }

            Err(e) => {
                self.events.notify(Event::IssueFetchFailed);
                info!("Error fetching {}!", job.pr.number);
                info!("E: {:?}", e);
                return self.actions().skip(&job);
            }
        };

        let mut evaluation_strategy: Box<eval::EvaluationStrategy> = if job.is_nixpkgs() {
            Box::new(eval::NixpkgsStrategy::new(&job, &repo, &pull, &issue_ref, &issue, &gists, &self.nix, &self.tag_paths)) /*, &self.events))*/
        } else {
            Box::new(eval::GenericStrategy::new())
        };

        let mut overall_status = CommitStatus::new(
            repo.statuses(),
            job.pr.head_sha.clone(),
            "grahamcofborg-eval".to_owned(),
            "Starting".to_owned(),
            None,
        );

        overall_status.set_with_description("Starting", hubcaps::statuses::State::Pending);

        evaluation_strategy.pre_clone().unwrap();

        let project = self
            .cloner
            .project(&job.repo.full_name, job.repo.clone_url.clone());

        overall_status.set_with_description("Cloning project", hubcaps::statuses::State::Pending);

        info!("Working on {}", job.pr.number);
        let co = project
            .clone_for("evaluate".to_string(), self.identity.clone())
            .unwrap();

        let target_branch = match job.pr.target_branch.clone() {
            Some(x) => x,
            None => String::from("master"),
        };

        overall_status.set_with_description(
            format!("Checking out {}", &target_branch).as_ref(),
            hubcaps::statuses::State::Pending,
        );
        info!("Checking out target branch {}", &target_branch);
        let refpath = co.checkout_origin_ref(target_branch.as_ref()).unwrap();


        evaluation_strategy.on_target_branch(&Path::new(&refpath), &mut overall_status);

        overall_status.set_with_description("Fetching PR", hubcaps::statuses::State::Pending);

        co.fetch_pr(job.pr.number).unwrap();

        if !co.commit_exists(job.pr.head_sha.as_ref()) {
            overall_status
                .set_with_description("Commit not found", hubcaps::statuses::State::Error);

            info!("Commit {} doesn't exist", job.pr.head_sha);
            return self.actions().skip(&job);
        }

        evaluation_strategy.after_fetch(&co);

        overall_status.set_with_description("Merging PR", hubcaps::statuses::State::Pending);

        if co.merge_commit(job.pr.head_sha.as_ref()).is_err() {
            overall_status
                .set_with_description("Failed to merge", hubcaps::statuses::State::Failure);

            info!("Failed to merge {}", job.pr.head_sha);

            evaluation_strategy.merge_conflict();

            return self.actions().skip(&job);
        }

        evaluation_strategy.after_merge(&mut overall_status);

        println!("Got path: {:?}, building", refpath);
        overall_status
            .set_with_description("Beginning Evaluations", hubcaps::statuses::State::Pending);

        let eval_results: bool = evaluation_strategy.evaluation_checks()
            .into_iter()
            .map(|check| {
                let mut status = CommitStatus::new(
                    repo.statuses(),
                    job.pr.head_sha.clone(),
                    check.name(),
                    check.cli_cmd(),
                    None,
                );

                status.set(hubcaps::statuses::State::Pending);

                let state: hubcaps::statuses::State;
                let gist_url: Option<String>;
                match check.execute(Path::new(&refpath)) {
                    Ok(_) => {
                        state = hubcaps::statuses::State::Success;
                        gist_url = None;
                    }
                    Err(mut out) => {
                        state = hubcaps::statuses::State::Failure;
                        gist_url = make_gist(
                            &gists,
                            &check.name(),
                            Some(format!("{:?}", state)),
                            file_to_str(&mut out),
                        );
                    }
                }

                status.set_url(gist_url);
                status.set(state.clone());

                if state == hubcaps::statuses::State::Success {
                    Ok(())
                } else {
                    Err(())
                }
            })
            .all(|status| status == Ok(()));

        let mut response: worker::Actions = vec![];
        if eval_results {
            match evaluation_strategy.all_evaluations_passed(Path::new(&refpath), &mut overall_status) {
                Ok(jobs) => {
                    for buildjobmsg in jobs {
                        for arch in auto_schedule_build_archs.iter() {
                            let (exchange, routingkey) = arch.as_build_destination();
                            response.push(worker::publish_serde_action(exchange, routingkey, &buildjobmsg));
                        }
                        response.push(worker::publish_serde_action(
                            Some("build-results".to_string()),
                            None,
                            &buildjob::QueuedBuildJobs {
                                job: buildjobmsg,
                                architectures: auto_schedule_build_archs
                                    .iter()
                                    .map(|arch| arch.to_string())
                                    .collect(),
                            },
                        ));
                    }

                    overall_status.set_with_description("^.^!", hubcaps::statuses::State::Success);
                },
                Err(_) => {
                    overall_status
                        .set_with_description("Complete, with errors", hubcaps::statuses::State::Failure);
                }
            }
        }

        self.events.notify(Event::TaskEvaluationCheckComplete);

        self.actions().done(&job, response)
    }
}

pub fn make_gist<'a>(
    gists: &hubcaps::gists::Gists<'a>,
    name: &str,
    description: Option<String>,
    contents: String,
) -> Option<String> {
    let mut files: HashMap<String, hubcaps::gists::Content> = HashMap::new();
    files.insert(
        name.to_string(),
        hubcaps::gists::Content {
            filename: Some(name.to_string()),
            content: contents,
        },
    );

    Some(
        gists
            .create(&hubcaps::gists::GistOptions {
                description,
                public: Some(true),
                files,
            })
            .expect("Failed to create gist!")
            .html_url,
    )
}

pub fn update_labels(issue: &hubcaps::issues::IssueRef, add: &[String], remove: &[String]) {
    let l = issue.labels();

    let existing: Vec<String> = issue
        .get()
        .unwrap()
        .labels
        .iter()
        .map(|l| l.name.clone())
        .collect();
    println!("Already: {:?}", existing);
    let to_add = add
        .iter()
        .filter(|l| !existing.contains(l)) // Remove labels already on the issue
        .map(|l| l.as_ref())
        .collect();
    info!("Adding labels: {:?}", to_add);

    let to_remove: Vec<String> = remove
        .iter()
        .filter(|l| existing.contains(l)) // Remove labels already on the issue
        .cloned()
        .collect();
    info!("Removing labels: {:?}", to_remove);

    l.add(to_add).expect("Failed to add tags");

    for label in to_remove {
        l.remove(&label).expect("Failed to remove tag");
    }
}

fn issue_is_wip(issue: &hubcaps::issues::Issue) -> bool {
    if issue.title.contains("[WIP]") {
        return true;
    }

    if issue.title.starts_with("WIP:") {
        return true;
    }

    issue.labels.iter().any(|label| indicates_wip(&label.name))
}

fn indicates_wip(text: &str) -> bool {
    let text = text.to_lowercase();

    if text.contains("work in progress") {
        return true;
    }

    if text.contains("work-in-progress") {
        return true;
    }

    false
}
