use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, NewPlan, NewTask, PlanStatus, Status};
use daruma_events::{Event, EventBus, EventEnvelope};
use daruma_shared::{AgentId, PlanId, ProjectId, TaskId};
use daruma_storage::{
    ActivityRepo, CommentRepo, Db, PlanRepo, ProjectRepo, RunRepo, SqliteEventStore, TaskRepo,
};

async fn stack() -> CommandHandler {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    CommandHandler::new(
        Arc::new(SqliteEventStore::new(pool.clone())),
        Arc::new(TaskRepo::new(pool.clone())),
        Arc::new(ProjectRepo::new(pool.clone())),
        Arc::new(CommentRepo::new(pool.clone())),
        Arc::new(ActivityRepo::new(pool.clone())),
        EventBus::default(),
    )
    .with_plans(Arc::new(PlanRepo::new(pool.clone())))
    .with_runs(Arc::new(RunRepo::new(pool)))
}

async fn execute(handler: &CommandHandler, command: Command) -> Vec<EventEnvelope> {
    handler.handle(command, Actor::user()).await.unwrap()
}

async fn plan(handler: &CommandHandler, parent: Option<PlanId>, active: bool) -> PlanId {
    let mut new = NewPlan::new("plan", ProjectId::new(), Actor::user());
    new.parent_plan_id = parent;
    let events = execute(
        handler,
        Command::CreatePlan {
            plan: new,
            external_ref: None,
        },
    )
    .await;
    let Event::PlanCreated { plan } = &events[0].payload else {
        panic!()
    };
    let id = plan.id;
    if active {
        execute(
            handler,
            Command::SetPlanStatus {
                plan_id: id,
                status: PlanStatus::Active,
                force: false,
                override_reason: None,
            },
        )
        .await;
    }
    id
}

async fn task(handler: &CommandHandler, plan_id: PlanId) -> TaskId {
    let events = execute(
        handler,
        Command::CreateTask {
            task: NewTask::new("task"),
        },
    )
    .await;
    let Event::TaskCreated { task } = &events[0].payload else {
        panic!()
    };
    let id = task.id.unwrap();
    execute(
        handler,
        Command::AddPlanTask {
            plan_id,
            task_id: id,
            position: None,
            depends_on: None,
        },
    )
    .await;
    id
}

fn status(id: TaskId, status: Status) -> Command {
    Command::SetStatus {
        id,
        status,
        force: false,
        override_reason: None,
        comment: None,
    }
}

fn closures(events: &[EventEnvelope]) -> Vec<PlanId> {
    events
        .iter()
        .filter_map(|event| match event.payload {
            Event::PlanStatusChanged {
                plan_id,
                to: PlanStatus::Completed,
                ..
            } => Some(plan_id),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn reconciles_once_across_concurrent_terminal_tasks_and_preserves_open_work() {
    let handler = stack().await;
    let id = plan(&handler, None, true).await;
    let a = task(&handler, id).await;
    let b = task(&handler, id).await;
    let (left, right) = tokio::join!(
        execute(&handler, Command::CompleteTask { id: a, note: None }),
        execute(&handler, status(b, Status::Cancelled)),
    );
    assert_eq!([closures(&left), closures(&right)].concat(), vec![id]);
    assert_eq!(
        handler
            .plans
            .as_ref()
            .unwrap()
            .get(id)
            .await
            .unwrap()
            .unwrap()
            .status,
        PlanStatus::Completed
    );
    assert!(execute(&handler, status(b, Status::Cancelled))
        .await
        .is_empty());

    let draft = plan(&handler, None, false).await;
    let draft_task = task(&handler, draft).await;
    assert!(closures(&execute(&handler, status(draft_task, Status::Done)).await).is_empty());
    let empty = plan(&handler, None, true).await;
    assert_eq!(
        handler
            .plans
            .as_ref()
            .unwrap()
            .get(empty)
            .await
            .unwrap()
            .unwrap()
            .status,
        PlanStatus::Active
    );
}

#[tokio::test]
async fn child_plans_and_runs_delay_closure_then_cascade_in_the_same_batch() {
    let handler = stack().await;
    let parent = plan(&handler, None, true).await;
    let parent_task = task(&handler, parent).await;
    let child = plan(&handler, Some(parent), true).await;
    let child_task = task(&handler, child).await;
    let events = execute(
        &handler,
        Command::StartRun {
            plan_id: child,
            agent_id: AgentId::new(),
            parent_run_id: None,
        },
    )
    .await;
    let Event::RunStarted { run } = &events[0].payload else {
        panic!()
    };
    let run_id = run.id;
    let events = execute(
        &handler,
        Command::BulkSetStatus {
            ids: vec![parent_task, child_task],
            status: Status::Done,
        },
    )
    .await;
    assert!(closures(&events).is_empty());
    let events = execute(&handler, Command::CompleteRun { run_id }).await;
    assert_eq!(closures(&events), vec![child, parent]);
    assert!(matches!(events[0].payload, Event::RunCompleted { .. }));

    let bulk = plan(&handler, None, true).await;
    let a = task(&handler, bulk).await;
    let b = task(&handler, bulk).await;
    assert_eq!(
        closures(
            &execute(
                &handler,
                Command::BulkSetStatus {
                    ids: vec![a, b],
                    status: Status::Cancelled,
                }
            )
            .await
        ),
        vec![bulk]
    );
}
