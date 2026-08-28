# Документация Daruma

## Структура

| Файл | Назначение |
|------|------------|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Полный архитектурный контракт (EN) — single source of truth |
| [architecture-policy.ru.md](architecture-policy.ru.md) | Зафиксированные policy-решения (RU): actors, cascade, sequence_id |
| [guides/ai-agent.md](guides/ai-agent.md) | Правила AI-слоя и обзор tools |
| [guides/mcp-client.md](guides/mcp-client.md) | Локальные файлы MCP-клиента (`~/.agents/daruma/`) |
| [guides/comment-conventions.md](guides/comment-conventions.md) | Префиксы в теле комментария (`lesson:`, `branch:`) |
| [mcp/EXECUTOR-LOOP.md](mcp/EXECUTOR-LOOP.md) | Канонический цикл drain plan → execute → complete |
| [adr/workspacegraph.md](adr/workspacegraph.md) | ADR WorkspaceGraph: sidecar index, nodes/edges, non-goals |
| Cursor rule `workspacegraph.mdc` | Guardrails для `daruma_workspacegraph_*` (граф — для связей/impact, не для списка задач). Ставится в `.cursor/rules/` командой `daruma-cursor install` вместе с `daruma-policy.mdc` и `daruma.mdc`. |
| [MODULES.md](MODULES.md) | Реестр модулей (core / client / transport / embed) |
| [MODULE_CONTRACT.md](MODULE_CONTRACT.md) | SLA между core и модулями |
| [RELEASES.md](RELEASES.md) | Контракт релизов OSS core и правила зависимостей apps |
| [VERSION_HISTORY.md](VERSION_HISTORY.md) | Контракт immutable version records для task/document changes |

## В корне репозитория (намеренно)

| Файл | Зачем в корне |
|------|----------------|
| [README.md](../README.md) | Точка входа GitHub / клонирования |
| [CHANGELOG.ru.md](../CHANGELOG.ru.md) | Keep a Changelog, виден в релизах |
| [CONTRIBUTING.md](../CONTRIBUTING.md) | Стандарт для PR и DCO |
| [CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md) | GitHub community |
| [LICENSE.commons-clause.md](../LICENSE.commons-clause.md) | Rider к Apache-2.0 |

Архитектура и гайды разработчика перенесены в `docs/`, чтобы в корне остались только «витринные» и community-файлы.

## Бэклог и планы

Роадмап и pending work ведутся в трекере мейнтейнеров; публичные
запросы и обсуждения — в
[GitHub issues](https://github.com/tupical/daruma/issues).

История: [CHANGELOG.ru.md](../CHANGELOG.ru.md) + `git log`.

## Feature requests

Перед изменениями event schema, REST/WS или MCP — откройте issue и
обсудите. Крупные фичи: сначала issue, потом PR.
