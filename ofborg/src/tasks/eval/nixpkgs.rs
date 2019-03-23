use ofborg::tagger::{
    MaintainerPRTagger, PathsTagger, PkgsAddedRemovedTagger, RebuildTagger, StdenvTagger,
};
use crate::maintainers;
use crate::maintainers::ImpactedMaintainers;
use hubcaps::repositories::Repository;
use ofborg::worker;
use ofborg::message::buildjob;
use ofborg::commentparser::Subset;
use uuid::Uuid;
use tasks::eval::{EvaluationStrategy, StepResult, Stdenvs, Error};
use hubcaps::issues::{IssueRef, Issue};
use hubcaps::pulls::PullRequest;
use hubcaps::gists::Gists;
use tasks::evaluate::{update_labels, make_gist};
use std::path::PathBuf;
use ofborg::outpathdiff::{OutPathDiff, OutPaths};
use std::time::Instant;
use std::path::Path;
use ofborg::files::file_to_str;
use ofborg::message::evaluationjob::EvaluationJob;
use ofborg::nix::Nix;
use ofborg::commitstatus::CommitStatus;
use ofborg::checkout::CachedProjectCo;
use std::collections::HashMap;
use ofborg::evalchecker::EvalChecker;
use ofborg::nix;
use ofborg::message::buildjob::BuildJob;

pub struct NixpkgsStrategy<'a> {
    job: &'a EvaluationJob,
    repo: &'a Repository<'a>,
    pull: &'a PullRequest<'a>,
    issue: &'a IssueRef<'a>,
    issue_data: &'a Issue,
    gists: &'a Gists<'a>,
    events: (),
    nix: &'a Nix,
    tag_paths: &'a HashMap<String, Vec<String>>,
    stdenvs: Option<Stdenvs>,
    outpathdiff: Option<OutPathDiff>,
    possibly_touched_packages: Option<Vec<String>>,
    changed_paths: Option<Vec<String>>,
}

impl <'a> NixpkgsStrategy<'a> {
    pub fn new(job: &'a EvaluationJob, repo: &'a Repository, pull: &'a PullRequest<'a>, issue: &'a IssueRef, issue_data: &'a Issue, gists: &'a Gists, nix: &'a Nix, tag_paths: &'a HashMap<String, Vec<String>>,) -> NixpkgsStrategy<'a> {
        Self {
            job,
            repo,
            pull,
            issue,
            issue_data,
            gists,
            nix,
            tag_paths,
            events: (),
            stdenvs: None,
            outpathdiff: None,
            possibly_touched_packages: None,
            changed_paths: None,
        }
    }

    fn tag_from_title(&self) {
        let darwin = self.issue.get()
            .map(|iss| {
                iss.title.to_lowercase().contains("darwin")
                    || iss.title.to_lowercase().contains("macos")
            })
            .unwrap_or(false);

        if darwin {
            update_labels(&self.issue, &[String::from("6.topic: darwin")], &[]);
        }
    }

    fn tag_from_paths(&self, issue: &hubcaps::issues::IssueRef, paths: &[String]) {
        let mut tagger = PathsTagger::new(self.tag_paths.clone());

        for path in paths {
            tagger.path_changed(&path);
        }

        update_labels(&issue, &tagger.tags_to_add(), &tagger.tags_to_remove());
    }

    fn check_meta(&self, co: &Path) -> StepResult<Vec<BuildJob>> {
        let mut status = CommitStatus::new(
            self.repo.statuses(),
            self.job.pr.head_sha.clone(),
            String::from("grahamcofborg-eval-check-meta"),
            String::from("config.nix: checkMeta = true"),
            None,
        );

        status.set(hubcaps::statuses::State::Pending);

        let state: hubcaps::statuses::State;
        let gist_url: Option<String>;

        let checker = OutPaths::new(self.nix.clone(), co.to_path_buf(), true);
        match checker.find() {
            Ok(pkgs) => {
                status.set_url(None);
                status.set(hubcaps::statuses::State::Success);

                if let Some(ref possibly_touched_packages) = self.possibly_touched_packages {
                    let mut try_build: Vec<String> = pkgs
                        .keys()
                        .map(|pkgarch| pkgarch.package.clone())
                        .filter(|pkg| possibly_touched_packages.contains(&pkg))
                        .collect();
                    try_build.sort();
                    try_build.dedup();

                    if !try_build.is_empty() && try_build.len() <= 10 {
                        // In the case of trying to merge master in to
                        // a stable branch, we don't want to do this.
                        // Therefore, only schedule builds if there
                        // less than or exactly 10
                        let msg = buildjob::BuildJob::new(
                            self.job.repo.clone(),
                            self.job.pr.clone(),
                            Subset::Nixpkgs,
                            try_build,
                            None,
                            None,
                            format!("{}", Uuid::new_v4()),
                        );

                        return Ok(vec![ msg ]);
                    }
                }

                return Ok(vec![]);
            }
            Err(mut out) => {
                state = hubcaps::statuses::State::Failure;
                gist_url = make_gist(
                    &self.gists,
                    "Meta Check",
                    Some(format!("{:?}", state)),
                    file_to_str(&mut out),
                );

                status.set_url(gist_url);
                status.set(state);
                return Err(Error::Fail(String::from("Failed to run verify package meta fields.")));
            }
        }
    }

    fn handle_changed_outputs(&mut self, co: &Path, overall_status: &mut CommitStatus) -> StepResult<()> {
        overall_status.set_with_description(
            "Calculating Changed Outputs",
            hubcaps::statuses::State::Pending,
        );

        let mut stdenvtagger = StdenvTagger::new();
        if let Some(ref stdenvs) = self.stdenvs {
            if !stdenvs.are_same() {
                stdenvtagger.changed(stdenvs.changed());
            }

            update_labels(
                &self.issue,
                &stdenvtagger.tags_to_add(),
                &stdenvtagger.tags_to_remove(),
            );
        }

        if let Some(rebuildsniff) = self.outpathdiff.take() {
            if let Some((removed, added)) = rebuildsniff.package_diff() {
                let mut addremovetagger = PkgsAddedRemovedTagger::new();
                addremovetagger.changed(&removed, &added);
                update_labels(
                    &self.issue,
                    &addremovetagger.tags_to_add(),
                    &addremovetagger.tags_to_remove(),
                );
            }

            if let Some(attrs) = rebuildsniff.calculate_rebuild() {
                let mut rebuild_tags = RebuildTagger::new();
                if !attrs.is_empty() {
                    let gist_url = make_gist(
                        &self.gists,
                        "Changed Paths",
                        Some("".to_owned()),
                        attrs
                            .iter()
                            .map(|attr| format!("{}\t{}", &attr.architecture, &attr.package))
                            .collect::<Vec<String>>()
                            .join("\n"),
                    );

                    overall_status.set_url(gist_url);

                    let changed_attributes = attrs
                        .iter()
                        .map(|attr| attr.package.split('.').collect::<Vec<&str>>())
                        .collect::<Vec<Vec<&str>>>();

                    if let Some(ref changed_paths) = self.changed_paths {
                        let m = ImpactedMaintainers::calculate(
                            &self.nix,
                            co,
                            &changed_paths,
                            &changed_attributes,
                        );

                        let gist_url = make_gist(
                            &self.gists,
                            "Potential Maintainers",
                            Some("".to_owned()),
                            match m {
                                Ok(ref maintainers) => format!("Maintainers:\n{}", maintainers),
                                Err(ref e) => format!("Ignorable calculation error:\n{:?}", e),
                            },
                        );

                        if let Ok(ref maint) = m {
                            request_reviews(&maint, &self.pull);
                            let mut maint_tagger = MaintainerPRTagger::new();
                            maint_tagger
                                .record_maintainer(&self.issue_data.user.login, &maint.maintainers_by_package());
                            update_labels(
                                &self.issue,
                                &maint_tagger.tags_to_add(),
                                &maint_tagger.tags_to_remove(),
                            );
                        }

                        let mut status = CommitStatus::new(
                            self.repo.statuses(),
                            self.job.pr.head_sha.clone(),
                            String::from("grahamcofborg-eval-check-maintainers"),
                            String::from("matching changed paths to changed attrs..."),
                            gist_url,
                        );
                        status.set(hubcaps::statuses::State::Success);
                    }
                }

                rebuild_tags.parse_attrs(attrs);

                update_labels(
                    &self.issue,
                    &rebuild_tags.tags_to_add(),
                    &rebuild_tags.tags_to_remove(),
                );
            }
        }

        Ok(())
    }
}

impl <'a> EvaluationStrategy for NixpkgsStrategy<'a> {
    fn pre_clone(&mut self) -> StepResult<()> {
        self.tag_from_title();
        Ok(())
    }

    fn on_target_branch(&mut self, co: &Path, status: &mut CommitStatus) -> StepResult<()> {
        status.set_with_description(
            "Checking original stdenvs",
            hubcaps::statuses::State::Pending,
        );

        let mut stdenvs = Stdenvs::new(self.nix.clone(), PathBuf::from(&co));
        stdenvs.identify_before();
        self.stdenvs = Some(stdenvs);

        let mut rebuildsniff = OutPathDiff::new(self.nix.clone(), PathBuf::from(&co));

        status.set_with_description(
            "Checking original out paths",
            hubcaps::statuses::State::Pending,
        );

        let target_branch = match self.job.pr.target_branch.clone() {
            Some(x) => x,
            None => String::from("master"),
        };

        let target_branch_rebuild_sniff_start = Instant::now();

        if let Err(mut output) = rebuildsniff.find_before() {
            status.set_url(make_gist(
                &self.gists,
                "Output path comparison",
                Some("".to_owned()),
                file_to_str(&mut output),
            ));

            /*
            self.events
                .notify(Event::TargetBranchFailsEvaluation(target_branch.clone()));
*/
            status.set_with_description(
                format!("Target branch {} doesn't evaluate!", &target_branch).as_ref(),
                hubcaps::statuses::State::Failure,
            );

            return Err(Error::Fail(String::from("Pull request targets a branch which does not evaluate!")))
        }
        self.outpathdiff = Some(rebuildsniff);

/*
        self.events.notify(Event::EvaluationDuration(
            target_branch.clone(),
            target_branch_rebuild_sniff_start.elapsed().as_secs(),
        ));
        self.events
            .notify(Event::EvaluationDurationCount(target_branch.clone()));
*/
        Ok(())
    }

    fn after_fetch(&mut self, co: &CachedProjectCo) -> StepResult<()> {
        self.possibly_touched_packages = Some(parse_commit_messages(
            &co.commit_messages_from_head(&self.job.pr.head_sha)
                .unwrap_or_else(|_| vec!["".to_owned()]),
        ));

        let changed_paths = co
            .files_changed_from_head(&self.job.pr.head_sha)
            .unwrap_or_else(|_| vec![]);
        self.tag_from_paths(&self.issue, &changed_paths);
        self.changed_paths = Some(changed_paths);

        Ok(())
    }

    fn merge_conflict(&mut self) {
        update_labels(&self.issue, &["2.status: merge conflict".to_owned()], &[]);
    }

    fn after_merge(&mut self, status: &mut CommitStatus) -> StepResult<()> {
        update_labels(&self.issue, &[], &["2.status: merge conflict".to_owned()]);


        if let Some(ref mut stdenvs) = self.stdenvs {
            status
                .set_with_description("Checking new stdenvs", hubcaps::statuses::State::Pending);
            stdenvs.identify_after();
        }

        if let Some(ref mut rebuildsniff) = self.outpathdiff {
            status
                .set_with_description("Checking new out paths", hubcaps::statuses::State::Pending);

            if let Err(mut output) = rebuildsniff.find_after() {
                return Err(Error::FailWithGist(
                    String::from("Failed to enumerate outputs after merging to {}"),
                    String::from("Output path comparison"),
                    file_to_str(&mut output)
                ))
            }
        }

        Ok(())
    }

    fn evaluation_checks(&self) -> Vec<EvalChecker> {
        vec![
            EvalChecker::new(
                "package-list",
                nix::Operation::QueryPackagesJSON,
                vec![String::from("--file"), String::from(".")],
                self.nix.clone(),
            ),
            EvalChecker::new(
                "package-list-no-aliases",
                nix::Operation::QueryPackagesJSON,
                vec![
                    String::from("--file"),
                    String::from("."),
                    String::from("--arg"),
                    String::from("config"),
                    String::from("{ allowAliases = false; }"),
                ],
                self.nix.clone(),
            ),
            EvalChecker::new(
                "nixos-options",
                nix::Operation::Instantiate,
                vec![
                    String::from("--arg"),
                    String::from("nixpkgs"),
                    String::from("{ outPath=./.; revCount=999999; shortRev=\"ofborg\"; }"),
                    String::from("./nixos/release.nix"),
                    String::from("-A"),
                    String::from("options"),
                ],
                self.nix.clone(),
            ),
            EvalChecker::new(
                "nixos-manual",
                nix::Operation::Instantiate,
                vec![
                    String::from("--arg"),
                    String::from("nixpkgs"),
                    String::from("{ outPath=./.; revCount=999999; shortRev=\"ofborg\"; }"),
                    String::from("./nixos/release.nix"),
                    String::from("-A"),
                    String::from("manual"),
                ],
                self.nix.clone(),
            ),
            EvalChecker::new(
                "nixpkgs-manual",
                nix::Operation::Instantiate,
                vec![
                    String::from("--arg"),
                    String::from("nixpkgs"),
                    String::from("{ outPath=./.; revCount=999999; shortRev=\"ofborg\"; }"),
                    String::from("./pkgs/top-level/release.nix"),
                    String::from("-A"),
                    String::from("manual"),
                ],
                self.nix.clone(),
            ),
            EvalChecker::new(
                "nixpkgs-tarball",
                nix::Operation::Instantiate,
                vec![
                    String::from("--arg"),
                    String::from("nixpkgs"),
                    String::from("{ outPath=./.; revCount=999999; shortRev=\"ofborg\"; }"),
                    String::from("./pkgs/top-level/release.nix"),
                    String::from("-A"),
                    String::from("tarball"),
                ],
                self.nix.clone(),
            ),
            EvalChecker::new(
                "nixpkgs-unstable-jobset",
                nix::Operation::Instantiate,
                vec![
                    String::from("--arg"),
                    String::from("nixpkgs"),
                    String::from("{ outPath=./.; revCount=999999; shortRev=\"ofborg\"; }"),
                    String::from("./pkgs/top-level/release.nix"),
                    String::from("-A"),
                    String::from("unstable"),
                ],
                self.nix.clone(),
            ),
        ]
    }

    fn all_evaluations_passed(&mut self, co: &Path, status: &mut CommitStatus) -> StepResult<Vec<BuildJob>> {
        let jobs = self.check_meta(co)?;
        self.handle_changed_outputs(co, status)?;
        return Ok(jobs);
    }
}


fn parse_commit_messages(messages: &[String]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|line| {
            // Convert "foo: some notes" in to "foo"
            let parts: Vec<&str> = line.splitn(2, ':').collect();
            if parts.len() == 2 {
                Some(parts[0])
            } else {
                None
            }
        })
        .flat_map(|line| {
            let pkgs: Vec<&str> = line.split(',').collect();
            pkgs
        })
        .map(|line| line.trim().to_owned())
        .collect()
}

fn request_reviews(maint: &maintainers::ImpactedMaintainers, pull: &hubcaps::pulls::PullRequest) {
    if maint.maintainers().len() < 10 {
        for maintainer in maint.maintainers() {
            if let Err(e) =
                pull.review_requests()
                    .create(&hubcaps::review_requests::ReviewRequestOptions {
                        reviewers: vec![maintainer.clone()],
                        team_reviewers: vec![],
                    })
            {
                println!("Failure requesting a review from {}: {:#?}", maintainer, e,);
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_parse_commit_messages() {
        let expect: Vec<&str> = vec![
            "firefox{-esr", // don't support such fancy syntax
            "}",            // Don't support such fancy syntax
            "firefox",
            "buildkite-agent",
            "python.pkgs.ptyprocess",
            "python.pkgs.ptyprocess",
            "android-studio-preview",
            "foo",
            "bar",
        ];
        assert_eq!(
            parse_commit_messages(
                &"
              firefox{-esr,}: fix failing build due to the google-api-key
              Merge pull request #34483 from andir/dovecot-cve-2017-15132
              firefox: enable official branding
              Merge pull request #34442 from rnhmjoj/virtual
              buildkite-agent: enable building on darwin
              python.pkgs.ptyprocess: 0.5 -> 0.5.2
              python.pkgs.ptyprocess: move expression
              Merge pull request #34465 from steveeJ/steveej-attempt-qtile-bump-0.10.7
              android-studio-preview: 3.1.0.8 -> 3.1.0.9
              Merge pull request #34188 from dotlambda/home-assistant
              Merge pull request #34414 from dotlambda/postfix
              foo,bar: something here: yeah
            "
                .lines()
                .map(|l| l.to_owned())
                .collect::<Vec<String>>(),
            ),
            expect
        );
    }
}
