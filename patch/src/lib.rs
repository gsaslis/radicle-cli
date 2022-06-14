#![allow(clippy::or_fun_call)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::for_kv_map)]
use std::convert::TryFrom;
use std::ffi::OsString;
use std::str::FromStr;

use anyhow::anyhow;

use common::cobs::patch::Verdict;
use librad::git::identities::local::LocalIdentity;
use librad::git::storage::ReadOnlyStorage;
use librad::git::Storage;
use librad::git_ext::{Oid, RefLike};
use librad::profile::Profile;

use radicle_common as common;
use radicle_common::args::{Args, Error, Help};
use radicle_common::cobs::patch::{MergeTarget, Patch, PatchId, Patches};
use radicle_common::cobs::{CobIdentifier, Store as _};
use radicle_common::{cobs, git, keys, patch, person, profile, project};
use radicle_terminal as term;
use radicle_terminal::patch::Comment;

pub const HELP: Help = Help {
    name: "patch",
    description: env!("CARGO_PKG_DESCRIPTION"),
    version: env!("CARGO_PKG_VERSION"),
    usage: r#"
Usage

    rad patch [<option>...]

Create options

    -u, --update [<id>]      Update an existing patch (default: no)
        --[no-]sync          Sync patch to seed (default: sync)
        --comment [<string>] Provide a comment to the patch or revision (default: prompt)
        --no-comment         Leave the patch or revision comment blank

Options

    -l, --list               List all patches (default: false)
        --help               Print help
"#,
};

pub const PATCH_MSG: &str = r#"
<!--
Please enter a patch message for your changes. An empty
message aborts the patch proposal.

The first line is the patch title. The patch description
follows, and must be separated with a blank line, just
like a commit message. Markdown is supported in the title
and description.
-->
"#;

pub const REVISION_MSG: &str = r#"
<!--
Please enter a comment for your patch update. Leaving this
blank is also okay.
-->
"#;

#[derive(Debug)]
pub enum Update {
    No,
    Any,
    Patch(CobIdentifier),
}

impl Default for Update {
    fn default() -> Self {
        Self::No
    }
}

#[derive(Default, Debug)]
pub struct Options {
    pub list: bool,
    pub verbose: bool,
    pub sync: bool,
    pub update: Update,
    pub comment: Comment,
}

impl Args for Options {
    fn from_args(args: Vec<OsString>) -> anyhow::Result<(Self, Vec<OsString>)> {
        use lexopt::prelude::*;

        let mut parser = lexopt::Parser::from_args(args);
        let mut list = false;
        let mut verbose = false;
        let mut sync = true;
        let mut comment = Comment::default();
        let mut update = Update::default();

        while let Some(arg) = parser.next()? {
            match arg {
                Long("list") | Short('l') => {
                    list = true;
                }
                Long("verbose") | Short('v') => {
                    verbose = true;
                }
                Long("comment") => {
                    comment = Comment::Text(parser.value()?.to_string_lossy().into());
                }
                Long("no-comment") => {
                    comment = Comment::Blank;
                }
                Long("update") | Short('u') => {
                    if let Ok(val) = parser.value() {
                        let val = val
                            .to_str()
                            .ok_or_else(|| anyhow!("patch id specified is not UTF-8"))?;
                        let id = CobIdentifier::from_str(val)
                            .map_err(|_| anyhow!("invalid patch id '{}'", val))?;

                        update = Update::Patch(id);
                    } else {
                        update = Update::Any;
                    }
                }
                Long("sync") => {
                    sync = true;
                }
                Long("no-sync") => {
                    sync = false;
                }
                Long("help") => {
                    return Err(Error::Help.into());
                }
                _ => return Err(anyhow::anyhow!(arg.unexpected())),
            }
        }

        Ok((
            Options {
                list,
                sync,
                comment,
                update,
                verbose,
            },
            vec![],
        ))
    }
}

pub fn run(options: Options) -> anyhow::Result<()> {
    let (urn, repo) = project::cwd()
        .map_err(|_| anyhow!("this command must be run in the context of a project"))?;

    let profile = profile::default()?;
    let signer = term::signer(&profile)?;
    let storage = keys::storage(&profile, signer)?;
    let project = project::get(&storage, &urn)?
        .ok_or_else(|| anyhow!("couldn't load project {} from local state", urn))?;

    if options.list {
        list(&storage, &repo, &profile, &project)?;
    } else {
        create(&storage, &profile, &project, &repo, options)?;
    }

    Ok(())
}

fn list(
    storage: &Storage,
    repo: &git::Repository,
    profile: &Profile,
    project: &project::Metadata,
) -> anyhow::Result<()> {
    let whoami = person::local(storage)?;
    let patches = cobs::patch::Patches::new(whoami.clone(), profile.paths(), storage)?;
    let proposed = patches.proposed(&project.urn)?;

    // Our `HEAD`.
    let head = repo.head()?;
    // Patches the user authored.
    let mut own = Vec::new();
    // Patches other users authored.
    let mut other = Vec::new();

    for (id, patch) in proposed {
        if *patch.author.urn() == whoami.urn() {
            own.push((id, patch));
        } else {
            other.push((id, patch));
        }
    }
    term::print(&term::format::badge_positive("YOU PROPOSED"));

    if own.is_empty() {
        term::blank();
        term::print(&term::format::italic("Nothing to show."));
    } else {
        for (id, patch) in &mut own {
            term::blank();

            print(&whoami, id, patch, project, &head, repo, storage)?;
        }
    }
    term::blank();
    term::print(&term::format::badge_secondary("OTHERS PROPOSED"));
    term::blank();

    if other.is_empty() {
        term::print(&term::format::italic("Nothing to show."));
    } else {
        for (id, patch) in &mut other {
            print(&whoami, id, patch, project, &head, repo, storage)?;
        }
    }
    term::blank();

    Ok(())
}

fn update(
    patch: Patch,
    patch_id: PatchId,
    head: &git::Oid,
    branch: &RefLike,
    patches: &Patches,
    project: &project::Metadata,
    repo: &git::Repository,
    options: Options,
) -> anyhow::Result<()> {
    let (current, current_revision) = patch.latest();

    if &*current_revision.oid == head {
        term::info!("Nothing to do, patch is already up to date.");
        return Ok(());
    }

    term::info!(
        "{} {} ({}) -> {} ({})",
        term::format::tertiary(common::fmt::cob(&patch_id)),
        term::format::dim(format!("R{}", current)),
        term::format::secondary(common::fmt::oid(&current_revision.oid)),
        term::format::dim(format!("R{}", current + 1)),
        term::format::secondary(common::fmt::oid(head)),
    );
    let comment = options.comment.get(REVISION_MSG);

    // Difference between the two revisions.
    term::patch::print_commits_ahead_behind(repo, *head, *current_revision.oid)?;
    term::blank();

    if !term::confirm("Continue?") {
        anyhow::bail!("patch update aborted by user");
    }

    let new = patches.update(&project.urn, &patch_id, comment, *head)?;
    assert_eq!(new, current + 1);

    term::blank();
    term::success!("Patch {} updated 🌱", term::format::highlight(patch_id));
    term::blank();

    if options.sync {
        rad_sync::run(rad_sync::Options {
            refs: rad_sync::Refs::Branch(branch.to_string()),
            verbose: options.verbose,
            ..rad_sync::Options::default()
        })?;
    }

    Ok(())
}

fn create(
    storage: &Storage,
    profile: &Profile,
    project: &project::Metadata,
    repo: &git::Repository,
    options: Options,
) -> anyhow::Result<()> {
    term::headline(&format!(
        "🌱 Creating patch for {}",
        term::format::highlight(&project.name)
    ));
    let whoami = person::local(storage)?;
    let patches = cobs::patch::Patches::new(whoami, profile.paths(), storage)?;

    // `HEAD`; This is what we are proposing as a patch.
    let head = repo.head()?;
    let head_oid = head.target().ok_or(anyhow!("invalid HEAD ref; aborting"))?;
    let head_commit = repo.find_commit(head_oid)?;
    let head_branch = head
        .shorthand()
        .ok_or(anyhow!("cannot create patch from detatched head; aborting"))?;
    let head_branch = RefLike::try_from(head_branch)?;

    // Make sure the `HEAD` commit can be found in the monorepo. Otherwise there
    // is no way for anyone to merge this patch.
    let spinner = term::spinner(format!(
        "Looking for HEAD ({}) in storage...",
        term::format::secondary(common::fmt::oid(&head_oid))
    ));
    if storage.find_object(Oid::from(head_oid))?.is_none() {
        spinner.failed();
        term::blank();

        return Err(Error::WithHint {
            err: anyhow!("Current branch head was not found in storage"),
            hint: "hint: run `git push rad` and try again",
        }
        .into());
    }
    spinner.finish();

    // Determine the merge target for this patch. This can ben any tracked remote's "default"
    // branch, as well as your own (eg. `rad/master`).
    let targets = patch::find_merge_targets(&head_oid, storage, project)?;

    // Show which peers have merged the patch.
    for peer in &targets.merged {
        term::info!(
            "{} {}",
            peer.name(),
            term::format::badge_secondary("merged")
        );
    }
    // eg. `refs/namespaces/<proj>/refs/remotes/<peer>/heads/master`
    let (target_peer, target_oid) = match targets.not_merged.as_slice() {
        [] => anyhow::bail!("no merge targets found for patch"),
        [target] => target,
        _ => {
            // TODO: Let user select which branch to use as a target.
            todo!();
        }
    };

    // The merge base is basically the commit at which the histories diverge.
    let merge_base_oid = repo.merge_base((*target_oid).into(), head_oid)?;
    let commits = patch::patch_commits(repo, &merge_base_oid, &head_oid)?;

    let patch = match &options.update {
        Update::No => None,
        Update::Any => {
            let mut spinner = term::spinner("Finding patches to update...");
            let mut result = find_unmerged_with_base(
                head_oid,
                **target_oid,
                merge_base_oid,
                &patches,
                &project.urn,
                repo,
            )?;

            if let Some((id, patch)) = result.pop() {
                if result.is_empty() {
                    spinner.message(format!(
                        "Found existing patch {} {}",
                        term::format::tertiary(common::fmt::cob(&id)),
                        term::format::italic(&patch.title)
                    ));
                    term::blank();

                    Some((id, patch))
                } else {
                    spinner.failed();
                    term::blank();
                    anyhow::bail!("More than one patch available to update, please specify an id with `rad patch --update <id>`");
                }
            } else {
                spinner.failed();
                term::blank();
                anyhow::bail!("No patches found that share a base, please create a new patch or specify the patch id manually");
            }
        }
        Update::Patch(identifier) => {
            let id = patches.resolve_id(&project.urn, identifier.clone())?;

            if let Some(patch) = patches.get(&project.urn, &id)? {
                Some((id, patch))
            } else {
                anyhow::bail!("Patch '{}' not found", id);
            }
        }
    };

    if let Some((id, patch)) = patch {
        if term::confirm("Update?") {
            term::blank();

            return update(
                patch,
                id,
                &head_oid,
                &head_branch,
                &patches,
                project,
                repo,
                options,
            );
        } else {
            anyhow::bail!("Patch update aborted by user");
        }
    }

    // TODO: List matching working copy refs for all targets.

    let user_name = storage.config_readonly()?.user_name()?;
    term::blank();
    term::info!(
        "{}/{} ({}) <- {}/{} ({})",
        target_peer.name(),
        term::format::highlight(&project.default_branch.to_string()),
        term::format::secondary(&common::fmt::oid(target_oid)),
        user_name,
        term::format::highlight(&head_branch.to_string()),
        term::format::secondary(&common::fmt::oid(&head_oid)),
    );

    // TODO: Test case where the target branch has been re-written passed the merge-base, since the fork was created
    // This can also happen *after* the patch is created.

    term::patch::print_commits_ahead_behind(repo, head_oid, (*target_oid).into())?;

    // List commits in patch that aren't in the target branch.
    term::blank();
    term::patch::list_commits(&commits)?;
    term::blank();

    if !term::confirm("Continue?") {
        anyhow::bail!("patch proposal aborted by user");
    }

    let message = head_commit
        .message()
        .ok_or(anyhow!("commit summary is not valid UTF-8; aborting"))?;
    let (title, description) = edit_message(message)?;
    let title_pretty = &term::format::dim(format!("╭─ {} ───────", title));

    term::blank();
    term::print(title_pretty);
    term::blank();

    if description.is_empty() {
        term::print(term::format::italic("No description provided."));
    } else {
        term::markdown(&description);
    }

    term::blank();
    term::print(&term::format::dim(format!(
        "╰{}",
        "─".repeat(term::text_width(title_pretty) - 1)
    )));
    term::blank();

    if !term::confirm("Create patch?") {
        anyhow::bail!("patch proposal aborted by user");
    }

    let id = patches.create(
        &project.urn,
        &title,
        &description,
        MergeTarget::default(),
        head_oid,
        &[],
    )?;

    term::blank();
    term::success!("Patch {} created 🌱", term::format::highlight(id));

    // TODO: Don't show "Project synced, you can find your project at ... etc."
    if options.sync {
        rad_sync::run(rad_sync::Options {
            refs: rad_sync::Refs::Branch(head_branch.to_string()),
            verbose: options.verbose,
            ..rad_sync::Options::default()
        })?;
    }

    Ok(())
}

fn edit_message(message: &str) -> anyhow::Result<(String, String)> {
    let message = match term::Editor::new()
        .require_save(true)
        .trim_newlines(true)
        .extension(".markdown")
        .edit(&format!("{}{}", message, PATCH_MSG))
        .unwrap()
    {
        Some(s) => s,
        None => anyhow::bail!("user aborted the patch"),
    };
    let (title, description) = message
        .split_once("\n\n")
        .ok_or(anyhow!("invalid title or description"))?;
    let (title, description) = (title.trim(), description.trim());
    let description = description.replace(PATCH_MSG.trim(), ""); // Delete help message.

    Ok((title.to_owned(), description))
}

/// Adds patch details as a new row to `table` and render later.
pub fn print(
    whoami: &LocalIdentity,
    patch_id: &PatchId,
    patch: &mut Patch,
    project: &project::Metadata,
    head: &git::Reference,
    repo: &git::Repository,
    storage: &Storage,
) -> anyhow::Result<()> {
    for r in patch.revisions.iter_mut() {
        for (_, r) in &mut r.reviews {
            r.author.resolve(storage).ok();
        }
    }
    patch.author.resolve(storage).ok();

    let revision = patch.revisions.last();
    let revision_oid = revision.oid;
    let revision_pretty = term::format::dim(format!("R{}", patch.version()));
    let you = patch.author.urn() == &whoami.urn();
    let prefix = "└── ";
    let mut author_info = vec![format!(
        "{}{} opened by {}",
        prefix,
        term::format::secondary(common::fmt::cob(patch_id)),
        term::format::tertiary(patch.author.name()),
    )];

    if you {
        author_info.push(term::format::secondary("(you)"));
    }
    author_info.push(term::format::dim(patch.timestamp));

    let diff = if let Some(head_oid) = head.target() {
        let (a, b) = repo.graph_ahead_behind(revision_oid.into(), head_oid)?;
        if a > 0 || b > 0 {
            let ahead = term::format::positive(a);
            let behind = term::format::negative(b);

            format!("ahead {}, behind {}", ahead, behind)
        } else {
            term::format::dim("up to date")
        }
    } else {
        String::default()
    };

    term::info!(
        "{} {} {} {}",
        term::format::bold(&patch.title),
        revision_pretty,
        term::format::secondary(common::fmt::oid(&revision_oid)),
        diff
    );
    term::info!("{}", author_info.join(" "));

    let mut timeline = Vec::new();
    for merge in &revision.merges {
        let peer = project::PeerInfo::get(&merge.peer, project, storage);
        let mut badges = Vec::new();

        if peer.delegate {
            badges.push(term::format::badge_secondary("delegate"));
        }
        if peer.id == *storage.peer_id() {
            badges.push(term::format::secondary("(you)"));
        }

        timeline.push((
            merge.timestamp,
            format!(
                "{}{} by {} {}",
                " ".repeat(term::text_width(prefix)),
                term::format::secondary(term::format::dim("✓ merged")),
                term::format::tertiary(peer.name()),
                badges.join(" "),
            ),
        ));
    }
    for (_, review) in &revision.reviews {
        let verdict = match review.verdict {
            Verdict::Accept => term::format::positive(term::format::dim("✓ accepted")),
            Verdict::Reject => term::format::negative(term::format::dim("✗ rejected")),
            Verdict::Pass => term::format::negative(term::format::dim("⋄ reviewed")),
        };
        let peer = project::PeerInfo::get(&review.author.peer, project, storage);
        let mut badges = Vec::new();

        if peer.delegate {
            badges.push(term::format::badge_secondary("delegate"));
        }
        if peer.id == *storage.peer_id() {
            badges.push(term::format::secondary("(you)"));
        }

        timeline.push((
            review.timestamp,
            format!(
                "{}{} by {} {}",
                " ".repeat(term::text_width(prefix)),
                verdict,
                term::format::tertiary(review.author.name()),
                badges.join(" "),
            ),
        ));
    }
    timeline.sort_by_key(|(t, _)| *t);

    for (time, event) in timeline.iter().rev() {
        term::info!("{} {}", event, term::format::dim(time));
    }

    Ok(())
}

/// Find patches with a merge base equal to the one provided.
fn find_unmerged_with_base(
    patch_head: git::Oid,
    target_head: git::Oid,
    merge_base: git::Oid,
    patches: &Patches,
    project: &common::Urn,
    repo: &git::Repository,
) -> anyhow::Result<Vec<(PatchId, Patch)>> {
    // My patches.
    let proposed: Vec<_> = patches
        .proposed_by(patches.whoami.urn(), project)?
        .collect();

    let mut matches = Vec::new();

    for (id, patch) in proposed {
        let (_, rev) = patch.latest();

        if !rev.merges.is_empty() {
            continue;
        }
        if **patch.head() == patch_head {
            continue;
        }
        // Merge-base between the two patches.
        if repo.merge_base(**patch.head(), target_head)? == merge_base {
            matches.push((id, patch));
        }
    }
    Ok(matches)
}
