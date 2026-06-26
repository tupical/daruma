import * as vscode from "vscode";
import { Plan, Project, Task, DarumaApiClient } from "./apiClient.js";

type Node = ProjectNode | PlanNode | TaskNode;

interface ProjectNode {
  kind: "project";
  project: Project;
}

interface PlanNode {
  kind: "plan";
  plan: Plan;
}

interface TaskNode {
  kind: "task";
  task: Task;
}

export class DarumaTreeProvider implements vscode.TreeDataProvider<Node> {
  private readonly onDidChangeTreeDataEmitter = new vscode.EventEmitter<Node | undefined>();
  readonly onDidChangeTreeData = this.onDidChangeTreeDataEmitter.event;

  constructor(private readonly client: DarumaApiClient) {}

  refresh(): void {
    this.onDidChangeTreeDataEmitter.fire(undefined);
  }

  async getChildren(element?: Node): Promise<Node[]> {
    if (!element) {
      const projects = await this.client.listProjects();
      if (projects.length === 0) {
        const tasks = await this.client.listTasks();
        return tasks.map((task) => ({ kind: "task", task }));
      }
      return projects.map((project) => ({ kind: "project", project }));
    }

    if (element.kind === "project") {
      const plans = await this.client.listPlans(element.project.id);
      if (plans.length > 0) {
        return plans.map((plan) => ({ kind: "plan", plan }));
      }
      const tasks = await this.client.listTasks(element.project.id);
      return tasks.map((task) => ({ kind: "task", task }));
    }

    if (element.kind === "plan") {
      const graph = await this.client.getPlanGraph(element.plan.id);
      return graph.nodes
        .sort((a, b) => a.position - b.position)
        .map((node) => ({
          kind: "task",
          task: { id: node.task_id, title: node.title, status: node.status, project_id: element.plan.project_id }
        }));
    }

    return [];
  }

  getTreeItem(element: Node): vscode.TreeItem {
    if (element.kind === "project") {
      const item = new vscode.TreeItem(element.project.title, vscode.TreeItemCollapsibleState.Collapsed);
      item.id = element.project.id;
      item.contextValue = "daruma.project";
      item.iconPath = new vscode.ThemeIcon("repo");
      return item;
    }

    if (element.kind === "plan") {
      const item = new vscode.TreeItem(element.plan.title, vscode.TreeItemCollapsibleState.Collapsed);
      item.id = element.plan.id;
      item.description = element.plan.status;
      item.contextValue = "daruma.plan";
      item.iconPath = new vscode.ThemeIcon("list-tree");
      item.command = {
        command: "daruma.openPlan",
        title: "Open Plan",
        arguments: [element.plan]
      };
      return item;
    }

    const item = new vscode.TreeItem(element.task.title, vscode.TreeItemCollapsibleState.None);
    item.id = element.task.id;
    item.description = [element.task.status, element.task.priority].filter(Boolean).join(" ");
    item.contextValue = "daruma.task";
    item.iconPath = taskIcon(element.task.status);
    item.command = {
      command: "daruma.showTask",
      title: "Show Task",
      arguments: [element.task]
    };
    return item;
  }
}

function taskIcon(status?: string): vscode.ThemeIcon {
  switch (status) {
    case "done":
      return new vscode.ThemeIcon("pass");
    case "in_progress":
      return new vscode.ThemeIcon("sync");
    case "in_review":
      return new vscode.ThemeIcon("eye");
    default:
      return new vscode.ThemeIcon("circle-outline");
  }
}
