//! Subcommand implementations for the CLI.

use std::str::FromStr;

use taskagent_core::embed::Command;
use taskagent_domain::{Actor, NewTask, Priority, Status};
use taskagent_shared::TaskId;

use crate::{context::Context, remote::HttpReplicaSink, render};

pub async fn list(ctx: &Context, args: &[String]) -> anyhow::Result<()> {
    let tasks = if let Some(filter) = args.first() {
        let status = parse_status(filter)?;
        ctx.tasks
            .list_by_status(status)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
    } else {
        ctx.tasks
            .list_all()
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
    };

    render::print_tasks(&tasks);
    if args.is_empty() {
        render::status_summary(&tasks);
    }
    Ok(())
}

pub async fn add(ctx: &Context, args: &[String]) -> anyhow::Result<()> {
    let title = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: taskagent add \"<title>\" [--p0|--p1|--p2|--p3]"))?
        .clone();

    let mut priority: Option<Priority> = None;
    for flag in &args[1..] {
        match flag.as_str() {
            "--p0" => priority = Some(Priority::P0),
            "--p1" => priority = Some(Priority::P1),
            "--p2" => priority = Some(Priority::P2),
            "--p3" => priority = Some(Priority::P3),
            other => anyhow::bail!("unknown flag: {other}"),
        }
    }

    let mut task = NewTask::new(title);
    task.priority = priority;

    let envelopes = ctx
        .local
        .dispatch(Command::CreateTask { task }, Actor::user())
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    for e in &envelopes {
        println!("✓ {} ({})", e.kind(), e.id);
    }
    Ok(())
}

pub async fn done(ctx: &Context, args: &[String]) -> anyhow::Result<()> {
    let raw_id = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: taskagent done <task_id>"))?;
    let id = parse_task_id(ctx, raw_id).await?;

    let envelopes = ctx
        .local
        .dispatch(Command::CompleteTask { id, note: None }, Actor::user())
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    for e in &envelopes {
        println!("✓ {}", e.kind());
    }
    Ok(())
}

pub async fn delete(ctx: &Context, args: &[String]) -> anyhow::Result<()> {
    let raw_id = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: taskagent delete <task_id>"))?;
    let id = parse_task_id(ctx, raw_id).await?;

    ctx.local
        .dispatch(Command::DeleteTask { id }, Actor::user())
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    println!("✓ deleted {id}");
    Ok(())
}

pub async fn sync(ctx: &Context, args: &[String]) -> anyhow::Result<()> {
    let limit = parse_limit(args)?;
    let sink = HttpReplicaSink::from_env().map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let stats = ctx
        .local
        .flush_pending(&sink, limit)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let catch_up = ctx
        .replica
        .catch_up(&sink, limit)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    println!(
        "flushed {}/{} pending event(s), applied {}/{} remote event(s), server_seq={}",
        stats.flushed, stats.attempted, catch_up.applied, catch_up.fetched, catch_up.server_seq
    );
    Ok(())
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn parse_limit(args: &[String]) -> anyhow::Result<u32> {
    if args.is_empty() {
        return Ok(100);
    }
    if args.len() == 2 && args[0] == "--limit" {
        let limit = args[1]
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("--limit must be a positive integer"))?;
        if limit == 0 {
            anyhow::bail!("--limit must be greater than zero");
        }
        return Ok(limit);
    }
    anyhow::bail!("usage: taskagent sync [--limit N]")
}

fn parse_status(s: &str) -> anyhow::Result<Status> {
    Ok(match s {
        "inbox" => Status::Inbox,
        "todo" => Status::Todo,
        "in_progress" | "doing" => Status::InProgress,
        "in_review" | "review" => Status::InReview,
        "done" => Status::Done,
        "cancelled" | "canceled" => Status::Cancelled,
        other => anyhow::bail!("unknown status: {other}"),
    })
}

/// Accept either a full prefixed ID (`tsk_<uuid>`) or a short prefix.
async fn parse_task_id(ctx: &Context, raw: &str) -> anyhow::Result<TaskId> {
    if let Ok(id) = TaskId::from_str(raw) {
        return Ok(id);
    }

    // Short-prefix lookup: scan tasks for one whose display starts with `raw`.
    let candidates = ctx
        .tasks
        .list_all()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let needle = if raw.starts_with("tsk_") {
        raw.to_owned()
    } else {
        format!("tsk_{raw}")
    };

    let matches: Vec<_> = candidates
        .iter()
        .filter(|t| t.id.to_string().starts_with(&needle))
        .collect();

    match matches.len() {
        0 => anyhow::bail!("no task matches: {raw}"),
        1 => Ok(matches[0].id),
        _ => anyhow::bail!("ambiguous task prefix: {raw} ({} matches)", matches.len()),
    }
}
