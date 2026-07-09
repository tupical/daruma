//! Tiny ASCII renderer for the CLI. No dependencies.

use daruma_domain::{Status, Task};

use crate::remote::Device;

const COL_ID: usize = 8;
const COL_STATUS: usize = 12;
const COL_PRIORITY: usize = 4;
const COL_DUE: usize = 16;

pub fn print_tasks(tasks: &[Task]) {
    if tasks.is_empty() {
        println!("(no tasks — add one with `daruma add \"…\"`)");
        return;
    }

    println!(
        "{:<id$}  {:<st$}  {:<pr$}  {:<due$}  TITLE",
        "ID",
        "STATUS",
        "PRI",
        "DUE",
        id = COL_ID,
        st = COL_STATUS,
        pr = COL_PRIORITY,
        due = COL_DUE,
    );
    println!(
        "{}",
        "─".repeat(COL_ID + COL_STATUS + COL_PRIORITY + COL_DUE + 16)
    );

    for t in tasks {
        let id = short_id(&t.id.to_string());
        let due = t
            .due_at
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "—".into());

        println!(
            "{:<id$}  {:<st$}  {:<pr$}  {:<due$}  {}",
            id,
            t.status.as_str(),
            t.priority.as_str(),
            due,
            t.title,
            id = COL_ID,
            st = COL_STATUS,
            pr = COL_PRIORITY,
            due = COL_DUE,
        );
    }
}

pub fn status_summary(tasks: &[Task]) {
    let mut inbox = 0;
    let mut todo = 0;
    let mut doing = 0;
    let mut review = 0;
    let mut done = 0;
    let mut cancelled = 0;
    for t in tasks {
        match t.status {
            Status::Inbox => inbox += 1,
            Status::Todo => todo += 1,
            Status::InProgress => doing += 1,
            Status::InReview => review += 1,
            Status::Done => done += 1,
            Status::Cancelled => cancelled += 1,
        }
    }
    println!(
        "  inbox: {inbox}   todo: {todo}   in_progress: {doing}   in_review: {review}   done: {done}   cancelled: {cancelled}"
    );
}

pub fn print_devices(current: Option<daruma_shared::DeviceId>, devices: &[Device]) {
    if devices.is_empty() {
        println!("(no paired devices)");
        return;
    }
    println!(
        "{:<12}  {:<20}  {:<10}  {:<19}  LABEL",
        "ID", "STATE", "CURRENT", "LAST SEEN"
    );
    println!("{}", "-".repeat(78));
    for device in devices {
        let state = if device.revoked_at.is_some() {
            "revoked"
        } else if device.connected {
            "connected"
        } else {
            "disconnected"
        };
        let current_marker = if Some(device.id) == current {
            "yes"
        } else {
            ""
        };
        println!(
            "{:<12}  {:<20}  {:<10}  {:<19}  {}",
            short_id(&device.id.to_string()),
            state,
            current_marker,
            human_time(device.last_seen_at.as_deref()),
            device.label,
        );
    }
}

/// Render an ID short enough to be readable in the table.
fn short_id(full: &str) -> String {
    // `tsk_<uuid>` — keep the prefix + first 4 hex chars.
    if let Some((prefix, rest)) = full.split_once('_') {
        let head: String = rest.chars().take(4).collect();
        format!("{prefix}_{head}")
    } else {
        full.chars().take(COL_ID).collect()
    }
}

fn human_time(ts: Option<&str>) -> String {
    let Some(ts) = ts else {
        return "never".to_string();
    };
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return ts.chars().take(19).collect();
    };
    dt.with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}
