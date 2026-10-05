//! The applier (§B.8 step 6): runs a plan's steps against a [`Target`] and
//! records the outcome on the state entries. Every step is idempotent and
//! re-derivable from the floe side, so a crash between a step and the state CAS
//! costs nothing but a repeat (the ownership and "who wrote it" rules below).

use std::collections::BTreeSet;

use floe_config::GithubMirrorConfig;
use floe_git::RepoId;

use crate::naming;
use crate::plan::Step;
use crate::settings;
use crate::source::Source;
use crate::state::{RepoEntry, Status};
use crate::target::{CreateOutcome, ExistingRepo, PublishOutcome, READ_ONLY_POLICY, Target};

/// What a step did to its repository's sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Go on with the next step.
    Continue,
    /// Stop this repository's steps (conflict, detached, too large and unclaimed).
    Stop,
    /// Save the state (the entry's new `claiming`), then run the same step again.
    Persist,
}

/// Counters a step contributes to the pass report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcome {
    pub created: bool,
    pub published: bool,
    pub nudged: bool,
}

pub struct Ctx<'a> {
    pub cfg: &'a GithubMirrorConfig,
    pub source: &'a dyn Source,
    pub target: &'a dyn Target,
    /// Floe repositories other state entries already map to: never claimed
    /// again, so two forge ids cannot end up on one floe repository.
    pub taken: BTreeSet<String>,
}

/// Run one step for entry `id`. An `Err` is the step's failure; the caller marks
/// the entry `error` and stops its sequence.
pub async fn step(
    ctx: &Ctx<'_>,
    id: &str,
    e: &mut RepoEntry,
    s: &Step,
    out: &mut Outcome,
) -> anyhow::Result<Flow> {
    match s {
        Step::Create { allow_create } => claim(ctx, id, e, *allow_create, out).await,
        Step::PutPolicy => {
            let rid = floe_id(e)?;
            ctx.target
                .put_policy_if_absent(&rid, READ_ONLY_POLICY.as_bytes())
                .await?;
            e.policy = true;
            Ok(Flow::Continue)
        }
        Step::Publish { reason } => publish(ctx, id, e, reason, out).await,
        Step::Nudge => {
            if e.status == Status::Active {
                ctx.target.nudge_follow(&floe_id(e)?);
                out.nudged = true;
            }
            Ok(Flow::Continue)
        }
    }
}

fn floe_id(e: &RepoEntry) -> anyhow::Result<RepoId> {
    e.floe
        .as_deref()
        .and_then(naming::parse)
        .ok_or_else(|| anyhow::anyhow!("no floe repository recorded"))
}

/// Ownership (§B.7.1): ours when it carries our marker (`upstream.source`), or
/// when it is the empty, never-written repository this entry recorded it was
/// about to create (`claiming`: a crash between the create and the first
/// publish). Anything else, an own repository above all, is never adopted or
/// modified.
fn is_ours(ex: &ExistingRepo, marker: &str, claimed: bool) -> bool {
    ex.source.as_deref() == Some(marker)
        || (claimed && ex.head_seq == 0 && ex.settings_revision == 0)
}

/// The floe repository `Create` claims for `e` (`None`: the forge name has no
/// valid floe form).
fn claim_target(e: &RepoEntry, id: &str) -> Option<RepoId> {
    let r = e.to_remote(id);
    naming::floe_id(&r.owner, &r.name)
}

/// Adopt or create the one floe name the forge name maps to; a name taken by
/// a repository that is not this entry's is a `conflict` (skipped, retried).
async fn claim(
    ctx: &Ctx<'_>,
    id: &str,
    e: &mut RepoEntry,
    allow_create: bool,
    out: &mut Outcome,
) -> anyhow::Result<Flow> {
    let marker = settings::marker(ctx.source.kind(), id);
    let Some(rid) = claim_target(e, id) else {
        let why = format!("{} has no valid floe repository name", e.full_name);
        return Ok(conflict(e, id, allow_create, why));
    };
    if ctx.taken.contains(&rid.to_string()) {
        let why = format!("floe repository {rid} already mirrors another repository");
        return Ok(conflict(e, id, allow_create, why));
    }
    let claimed = e.claiming.as_deref() == Some(rid.to_string().as_str());
    let mut existing = ctx.target.exists(&rid).await?;
    if existing.is_none() && allow_create && !claimed {
        e.claiming = Some(rid.to_string());
        return Ok(Flow::Persist);
    }
    if existing.is_none() && allow_create {
        match ctx.target.create(&rid).await? {
            CreateOutcome::Created => {
                tracing::info!(id, floe = %rid, repo = %e.full_name, "created");
                out.created = true;
                e.floe = Some(rid.to_string());
                e.claiming = None;
                e.status = Status::Active;
                return Ok(Flow::Continue);
            }
            // Raced (or cached on this instance): judge what is there.
            CreateOutcome::AlreadyExists => existing = ctx.target.exists(&rid).await?,
        }
    }
    match existing {
        Some(ex) if is_ours(&ex, &marker, claimed) => {
            tracing::info!(id, floe = %rid, repo = %e.full_name, "adopted");
            e.floe = Some(rid.to_string());
            e.claiming = None;
            e.status = Status::Active;
            Ok(Flow::Continue)
        }
        Some(_) => {
            let why = format!(
                "floe repository {rid} exists and is not a mirror of {}: skipped, never adopted or overwritten",
                e.full_name
            );
            Ok(conflict(e, id, allow_create, why))
        }
        // Too large and nobody prepared it: stays too-large.
        None => Ok(Flow::Stop),
    }
}

/// The name is not ours to take: `conflict` (when the mirror may create; a
/// too-large entry stays too-large), with the reason in `last_error` for
/// `floe github status`. Retried every pass; never renamed around.
fn conflict(e: &mut RepoEntry, id: &str, allow_create: bool, why: String) -> Flow {
    e.claiming = None;
    if allow_create {
        tracing::warn!(id, repo = %e.full_name, "{why}");
        e.status = Status::Conflict;
        e.last_error = Some(why);
    }
    Flow::Stop
}

/// Publish the rendered table, after deciding who wrote the current one.
async fn publish(
    ctx: &Ctx<'_>,
    id: &str,
    e: &mut RepoEntry,
    reason: &str,
    out: &mut Outcome,
) -> anyhow::Result<Flow> {
    let rid = floe_id(e)?;
    let remote = e.to_remote(id);
    let desired = settings::render(ctx.source, ctx.cfg, &remote, e.status);
    let desired_hash = settings::hash(&desired);
    let description = settings::description(ctx.source, &remote);
    let ex = ctx
        .target
        .exists(&rid)
        .await?
        .ok_or_else(|| anyhow::anyhow!("floe repository {rid} no longer exists"))?;
    let current = settings::upstream_of(&ex.settings_toml);
    let current_hash = settings::hash(&current);
    let marker = settings::marker(ctx.source.kind(), id);
    let unchanged = e.settings_sha.as_deref() == Some(current_hash.as_str());
    // The mirror wrote it for this id (or with no marker) and lost the state
    // record, or it already is what we want. A table the mirror wrote for
    // another id is not ours.
    let ours_lost = (ex.settings_author == settings::AUTHOR
        && ex.source.as_deref().is_none_or(|s| s == marker))
        || current_hash == desired_hash;
    let adoptable = e.settings_sha.is_none()
        && (ex.settings_revision == 0 || ex.source.as_deref() == Some(marker.as_str()));
    if !(unchanged || ours_lost || adoptable) {
        tracing::warn!(id, floe = %rid, author = %ex.settings_author, "[upstream] was edited by someone else; detached (the mirror no longer touches it)");
        e.status = Status::Detached;
        e.last_error = Some(format!("[upstream] edited by {}", ex.settings_author));
        return Ok(Flow::Stop);
    }
    if current_hash == desired_hash && ex.description.as_deref() == Some(description.as_str()) {
        e.settings_sha = Some(current_hash);
        e.settings_revision = ex.settings_revision;
        return Ok(Flow::Continue);
    }
    match ctx
        .target
        .publish_upstream(&rid, &desired, &description, ex.settings_revision, reason)
        .await?
    {
        PublishOutcome::Published(rev) => {
            tracing::info!(id, floe = %rid, revision = rev, reason, "[upstream] published");
            e.settings_sha = Some(desired_hash);
            e.settings_revision = rev;
            out.published = true;
            Ok(Flow::Continue)
        }
        PublishOutcome::Conflict => {
            anyhow::bail!("settings of {rid} changed while publishing; retried next pass")
        }
    }
}
