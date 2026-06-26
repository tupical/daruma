import * as vscode from "vscode";
import { Plan, Task, DarumaApiClient } from "./apiClient.js";
import { DarumaTreeProvider } from "./tree.js";

export function activate(context: vscode.ExtensionContext): void {
  const client = createClient();
  const treeProvider = new DarumaTreeProvider(client);
  const tree = vscode.window.createTreeView("daruma.tasks", {
    treeDataProvider: treeProvider,
    showCollapseAll: true
  });
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  status.text = "$(checklist) Daruma";
  status.command = "daruma.refresh";
  status.show();

  let eventCursor: string | undefined;

  const refresh = async () => {
    treeProvider.refresh();
    await updateStatus(status, client);
  };

  const pollEvents = async () => {
    try {
      const response = await client.getEventsSince(eventCursor);
      if (response.events.length > 0) {
        treeProvider.refresh();
        await updateStatus(status, client);
      }
      if (response.cursor) {
        eventCursor = response.cursor;
      }
    } catch {
      // events/since endpoint may not be available — fall through to full refresh
      treeProvider.refresh();
      await updateStatus(status, client);
    }
  };

  const timer = setInterval(() => {
    void pollEvents();
  }, 5_000);

  context.subscriptions.push(
    tree,
    status,
    { dispose: () => clearInterval(timer) },
    vscode.commands.registerCommand("daruma.refresh", refresh),

    vscode.commands.registerCommand("daruma.completeTask", async (task?: Task) => {
      if (!task?.id) {
        return;
      }
      await client.completeTask(task.id);
      treeProvider.refresh();
    }),

    vscode.commands.registerCommand("daruma.claimTask", async (task?: Task) => {
      if (!task?.id) {
        return;
      }
      await client.claimTask(task.id);
      treeProvider.refresh();
    }),

    vscode.commands.registerCommand("daruma.commentTask", async (task?: Task) => {
      if (!task?.id) {
        return;
      }
      const comment = await vscode.window.showInputBox({
        title: `Comment on: ${task.title}`,
        prompt: "Enter your comment"
      });
      if (comment === undefined || comment.trim() === "") {
        return;
      }
      await client.commentTask(task.id, comment.trim());
      treeProvider.refresh();
    }),

    vscode.commands.registerCommand("daruma.setTaskPriority", async (task?: Task) => {
      if (!task?.id) {
        return;
      }
      const priority = await vscode.window.showQuickPick(["p0", "p1", "p2", "p3"], {
        title: `Set priority for: ${task.title}`,
        placeHolder: "Select priority"
      });
      if (!priority) {
        return;
      }
      await client.setTaskPriority(task.id, priority);
      treeProvider.refresh();
    }),

    vscode.commands.registerCommand("daruma.splitTask", async (task?: Task) => {
      if (!task?.id) {
        return;
      }
      const input = await vscode.window.showInputBox({
        title: `Split: ${task.title}`,
        prompt: "Enter subtask titles separated by semicolons",
        placeHolder: "Subtask A; Subtask B; Subtask C"
      });
      if (input === undefined || input.trim() === "") {
        return;
      }
      const titles = input
        .split(";")
        .map((s) => s.trim())
        .filter(Boolean);
      if (titles.length < 2) {
        await vscode.window.showWarningMessage("Please provide at least two subtask titles.");
        return;
      }
      await client.splitTask(task.id, titles);
      treeProvider.refresh();
    }),

    vscode.commands.registerCommand("daruma.showTask", async (task: Task) => {
      await vscode.window.showInformationMessage(`${task.title} (${task.status ?? "unknown"})`);
    }),

    vscode.commands.registerCommand("daruma.openPlan", async (plan?: Plan) => {
      if (!plan?.id) {
        return;
      }
      const detail = await client.getPlan(plan.id);
      const panel = vscode.window.createWebviewPanel(
        "daruma.plan",
        detail.plan.title,
        vscode.ViewColumn.One,
        { enableScripts: false }
      );
      const pct = Math.round(detail.progress.completion_pct ?? 0);
      const done = detail.progress.tasks_done ?? 0;
      const total = detail.progress.tasks_total ?? 0;
      panel.webview.html = `<!doctype html>
<html>
  <body>
    <h1>${escapeHtml(detail.plan.title)}</h1>
    <p>Status: ${escapeHtml(detail.plan.status ?? "unknown")}</p>
    <progress value="${pct}" max="100"></progress>
    <p>${pct}% complete (${done}/${total} tasks)</p>
  </body>
</html>`;
    })
  );

  void refresh();
  registerCursorMcpServer();
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (ch) => {
    switch (ch) {
      case "&":
        return "&amp;";
      case "<":
        return "&lt;";
      case ">":
        return "&gt;";
      case "\"":
        return "&quot;";
      default:
        return "&#39;";
    }
  });
}

async function updateStatus(status: vscode.StatusBarItem, client: DarumaApiClient): Promise<void> {
  try {
    const tasks = await client.listTasks();
    const open = tasks.filter((task) => task.status !== "done" && task.status !== "cancelled").length;
    status.text = `$(checklist) Daruma ${open}`;
    status.tooltip = `${open} open Daruma task(s)`;
  } catch {
    status.text = "$(warning) Daruma";
    status.tooltip = "Daruma server is unreachable";
  }
}

export function deactivate(): void {}

function createClient(): DarumaApiClient {
  const config = vscode.workspace.getConfiguration("daruma");
  const apiUrl = config.get<string>("apiUrl") || process.env.DARUMA_API_URL || "http://localhost:8080";
  const token = config.get<string>("token") || process.env.DARUMA_TOKEN || "";
  return new DarumaApiClient(apiUrl, token);
}

function registerCursorMcpServer(): void {
  try {
    const cursor = (vscode as unknown as { cursor?: { mcp?: { registerServer?: (server: unknown) => void } } }).cursor;
    const registerServer = cursor?.mcp?.registerServer;
    if (!registerServer) {
      return;
    }
    const config = vscode.workspace.getConfiguration("daruma");
    const apiUrl = config.get<string>("apiUrl") || process.env.DARUMA_API_URL || "http://localhost:8080";
    const token = config.get<string>("token") || process.env.DARUMA_TOKEN || "";
    registerServer({
      name: "daruma",
      server: {
        url: `${apiUrl.replace(/\/$/, "")}/v1/mcp`,
        headers: token ? { Authorization: `Bearer ${token}` } : undefined
      }
    });
  } catch {
    // cursor.mcp.registerServer is not available in all environments — ignore
  }
}
