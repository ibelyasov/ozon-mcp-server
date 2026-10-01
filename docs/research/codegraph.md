# CodeGraph: оценка для Ozon MCP

Проверено 2026-10-01 непосредственно в рабочем репозитории.

## Вывод

CodeGraph полезен как дополнительный инструмент навигации: быстро собирает исходники нескольких связанных функций и находит многие непосредственные вызовы. Для этого проекта польза умеренная. Точность графа недостаточна для самостоятельного определения последствий изменения или выбора тестов: локальная проверка выявила пропуски настоящих вызовов и ложные связи между одноимёнными методами.

В одном A/B на задаче об `ozon_get_reviews` агент с CodeGraph использовал на 7,0% больше обработанных токенов и на 22,1% больше токенов входа без кэша плюс выхода. Время выросло на 10,1%; качество ответов оказалось практически одинаковым. Экономии на этой задаче не получено. Одна пара прогонов не определяет результат для других задач; стоимость и расход лимита подписки не измерялись.

## Установленное состояние

- CLI **1.6.1**, официальный macOS ARM64 bundle: `~/.codegraph/versions/v1.6.1`; команда `~/.local/bin/codegraph` доступна в PATH. SHA-256 архива сопоставлен с digest публичного [релиза](https://github.com/colbymchenry/codegraph/releases/tag/v1.6.1): `7a08cf8cf26cdf9e4ba5f8b2a36bb9c039b8966a6cb30b5e27c4b82074abf499`. Локальная фиксация: [metadata.json](../../.work/codegraph-evaluation/2026-10-01/metadata.json).
- MCP зарегистрирован глобально в Codex через `codex mcp add`; запускает `/Users/ibelyasov/.local/bin/codegraph serve --mcp`. Поддержка stdio-команды и `--env` проверена в справке установленного Codex CLI 0.159.2 и [официальной документации](https://learn.chatgpt.com/docs/developer-commands#codex-mcp).
- Телеметрия отключена сохранённой настройкой `codegraph telemetry off`. В окружении MCP дополнительно заданы `CODEGRAPH_TELEMETRY=0`, `DO_NOT_TRACK=1`, `CODEGRAPH_NO_UPDATE_CHECK=1`. По исходникам телеметрия включена по умолчанию; выключение предусмотрено явно: [telemetry/index.ts](https://github.com/colbymchenry/codegraph/blob/v1.6.1/src/telemetry/index.ts#L184-L205).
- Индекс находится в `.codegraph/` этого проекта; каталог добавлен в корневой `.gitignore`. После испытания CodeGraph оставлен для постоянной навигации: в AGENTS.md добавлено короткое правило использования, обслуживание описано в [docs/agents/codegraph.md](../agents/codegraph.md). Используются стандартные фильтры и watcher; отдельные `codegraph.json`, проектная копия глобального MCP и Git hooks не добавлялись.
- В текущем сеансе MCP проверен отдельными реальными stdio-клиентами. Наличие инструмента в интерфейсе уже запущенного Codex не проверялось; для дальнейшей работы следует открыть новую сессию.

Проверенный проект: `183664385f06ee47de0a3d43417ecdd2f1f62748`. Установленный релиз соответствует upstream-тегу `v1.6.1` (`f4ddf508516332419ea3c95702810765936cf679`). Также изучен текущий upstream main `34ede4de888c55aab170c3d6349c60d8dc56e7f2`; ссылки на устройство установленной версии ниже закреплены за релизом.

После постоянной настройки повторный `init --yes` подтвердил существующий индекс, `sync` — актуальность. Контрольные init/sync/status/explore заняли соответственно 0,097/0,321/0,267/0,239 с: [cli-results.json](../../.work/codegraph-evaluation/2026-10-01/project-setup/cli-results.json). Реальный stdio-клиент использовал сохранённую глобальную MCP-конфигурацию: initialize 0,106 с, tools/list 0,001 с, контрольный explore 6,787 с, все ответы успешны: [mcp-results.json](../../.work/codegraph-evaluation/2026-10-01/project-setup/mcp-results.json). Итоговый индекс complete, 38 файлов, 1 252 узла, 4 519 связей, ноль pending changes: [final-status.json](../../.work/codegraph-evaluation/2026-10-01/project-setup/final-status.json). Эти времена относятся к отдельной проверке подключения после настройки, без LLM.

## Что измерено

| Проверка | Результат | Артефакт |
| --- | --- | --- |
| Первичная индексация | 2,107 с целиком; движок сообщил 892 мс | [init-result.json](../../.work/codegraph-evaluation/2026-10-01/init-result.json), [init.log](../../.work/codegraph-evaluation/2026-10-01/init.log) |
| Размер графа | 38 файлов, 1 252 узла, 4 519 связей; база 6 393 856 байт до пробного исходника | [status.stdout](../../.work/codegraph-evaluation/2026-10-01/status.stdout) |
| 20 CLI-проверок | Все команды завершились успешно; запросы кроме status: 0,119–0,238 с, медиана 0,125 с | [cli-results.json](../../.work/codegraph-evaluation/2026-10-01/cli-results.json) |
| MCP handshake и перечень инструментов | Три клиента; по умолчанию опубликован `codegraph_explore` | [mcp-tools.json](../../.work/codegraph-evaluation/2026-10-01/mcp-tools.json) |
| Первый explore через MCP | 0,485 с после handshake | [mcp-results.json](../../.work/codegraph-evaluation/2026-10-01/mcp-results.json) |
| Три параллельных explore | 0,084 с на всю волну; отдельные ответы 0,032–0,083 с | [mcp-results.json](../../.work/codegraph-evaluation/2026-10-01/mcp-results.json) |
| Создание, изменение и удаление исходника | Следующий поисковый запрос увидел каждое изменение через 0,50–0,52 с | [watch-create.json](../../.work/codegraph-evaluation/2026-10-01/watch-create.json), [watch-modify.json](../../.work/codegraph-evaluation/2026-10-01/watch-modify.json), [watch-remove.json](../../.work/codegraph-evaluation/2026-10-01/watch-remove.json) |

Это один небольшой функциональный прогон. Время запросов включает локальный клиент/процесс, но не работу LLM. Проверка обновления включала запросы к индексу и не отделяет работу watcher от согласования индекса перед запросом. Первый ответ при создании файла содержал корректное предупреждение о незавершённой индексации. Пробный `.rs` не подключался к Cargo; его окончательные байты сохранены в [watch-probe.rs](../../.work/codegraph-evaluation/2026-10-01/watch-probe.rs). Итоговый статус снова показывает 38 файлов и отсутствие ожидающих изменений: [mcp-final-status.json](../../.work/codegraph-evaluation/2026-10-01/mcp-final-status.json).

## Практическая польза

1. `callers prepare_reply` правильно нашёл пять production-функций: `get_images`, `commit_search`, `commit_reviews`, `get_context`, `products`. Это удобно перед изменением общего формирования ответа: [callers-reply.stdout](../../.work/codegraph-evaluation/2026-10-01/callers-reply.stdout), [application.rs](../../src/research/application.rs).
2. Внутри `commit_search` найдены `Journal::commit_operation`, `JournalTxn::put_ref/record_search`, `validate_evidence`, `validate_candidates`, `prepare_reply`, включая вызовы внутри транзакционного замыкания: [callees-commit.stdout](../../.work/codegraph-evaluation/2026-10-01/callees-commit.stdout), [application.rs](../../src/research/application.rs).
3. `explore "ozonPage page_script evaluate_script BrowserSession.evaluate"` одним ответом собрал участки authored TypeScript, Rust-обёртки и CDP. Это полезная подборка контекста, хотя она не доказывает существование межъязыковых связей: [flow-cross-language.stdout](../../.work/codegraph-evaluation/2026-10-01/flow-cross-language.stdout).

У проекта уже есть описание слоёв и пути запроса в [docs/behavior.md](../behavior.md), словарь в [CONTEXT.md](../../CONTEXT.md) и решение об атомарности журнала в [ADR 0001](../adr/0001-v3-contract-and-journal.md). Поэтому выигрыш вероятнее при поиске внутри крупных `application.rs` и `journal.rs`, чем при первичном изучении архитектуры.

## Подтверждённые ограничения на нашем коде

| Случай | Что показала проверка |
| --- | --- |
| Ложные вызовы `.execute()` | `callers PreparedSearch.execute` включил `insert_record`, `insert_event`, `enforce_capacity_tx_with_limit` из SQLite-журнала. Эти функции вызывают SQL `execute`, а не поиск Ozon. [Ответ графа](../../.work/codegraph-evaluation/2026-10-01/callers-prepared.stdout), [journal.rs](../../src/research/journal.rs). |
| Ложный путь из build script | `build.rs::main` связан с нашим `response::success`, хотя код вызывает `compiler.status.success()` стандартной библиотеки. Это затем загрязняет impact `validate_output`. [Прямые связи](../../.work/codegraph-evaluation/2026-10-01/callees-build.stdout), [impact](../../.work/codegraph-evaluation/2026-10-01/impact-validation.stdout), [build.rs](../../build.rs). |
| Пропущенный вызов через generic trait | `PreparedSearch::execute` вызывает `source.fetch_json` в `search.rs:303`, но полный callee-ответ с `truncated:false` не содержит `fetch_json`. [Ответ](../../.work/codegraph-evaluation/2026-10-01/callees-prepared.stdout), [search.rs](../../src/ozon/search.rs). |
| Пропущенные обычные вызовы | Отсутствуют `commit_search → observations::normalize_search` и `prepare_reply → response::success`, хотя они явно присутствуют в коде. [Первый ответ](../../.work/codegraph-evaluation/2026-10-01/callees-commit.stdout), [второй ответ](../../.work/codegraph-evaluation/2026-10-01/callees-reply.stdout), [application.rs](../../src/research/application.rs). |
| Не обнаружены inline Rust-тесты | `affected src/contracts.rs` вернул `affectedTests: []`. При этом `callers validate_output` находит функции с `#[test]`, прямо вызывающие валидатор, включая проверку rollback. [Affected](../../.work/codegraph-evaluation/2026-10-01/affected-contracts.stdout), [callers](../../.work/codegraph-evaluation/2026-10-01/callers-validation.stdout), [contracts.rs](../../src/contracts.rs), [journal.rs](../../src/research/journal.rs). |
| Потеря Rust module scope | Production `ozon::pages::Pages` и тестовый `ozon::source::tests::Pages` получили одинаковое имя `Pages::fetch_json`; запрос становится неоднозначным. [Ответ](../../.work/codegraph-evaluation/2026-10-01/callees-pages.stdout), [source.rs](../../src/ozon/source.rs). |
| Встроенный JavaScript | Callees `page_script` и callers `ozonPage` пусты. В нашем коде связь проходит через `include_str!` и выполнение строки. Подборка explore по явно указанным именам не заменяет эту отсутствующую связь. [page_script](../../.work/codegraph-evaluation/2026-10-01/callees-page-script.stdout), [ozonPage](../../.work/codegraph-evaluation/2026-10-01/callers-ozon-page.stdout), [pages.rs](../../src/ozon/pages.rs). |

Причина ожидаема: CodeGraph разбирает AST через tree-sitter и разрешает имена эвристически. Это не семантический граф rustc/rust-analyzer. Сам MCP-сервер признаёт best-effort name matching: [server-instructions.ts](https://github.com/colbymchenry/codegraph/blob/v1.6.1/src/mcp/server-instructions.ts#L72-L77); Rust extractor упрощает generic и scoped имена: [rust.ts](https://github.com/colbymchenry/codegraph/blob/v1.6.1/src/extraction/languages/rust.ts#L5-L74).

У интеграции есть дополнительная особенность: в MCP `initialize` CodeGraph передаёт агенту инструкцию «Trust codegraph's results — don't re-verify them with grep». Она получена всеми тремя клиентами: [mcp-init-0.json](../../.work/codegraph-evaluation/2026-10-01/mcp-init-0.json); [текст upstream-инструкций](https://github.com/colbymchenry/codegraph/blob/v1.6.1/src/mcp/server-instructions.ts#L63-L77). Наши результаты показывают, что критические зависимости всё же требуют проверки по исходникам. Встроенную подсказку о доверии графу следует учитывать как ограничение интеграции.

## A/B: токены и качество на одной задаче

Задача: объяснить продолжение `ozon_get_reviews` от MCP-диспетчеризации до приобретения страницы и атомарного сохранения, включая Product Reference/Cursor, Research/Context, остаток захваченной страницы, идентичность и дедупликацию, checkpoints, полноту источника, отмену и минимум три существующих теста. Требовались ссылки на строки кода и ответ до 900 слов. Полное условие: [task.txt](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/task.txt).

Два новых native-агента `researcher`, без истории родительского разговора (`fork_turns: none`), работали в том же репозитории параллельно. У обоих фактически использовались **gpt-6.1-sol / medium**, одинаковое условие и неизменные production-исходники. CodeGraph-агент должен был преимущественно получать исходники установленным CLI; разрешались адресные чтения для недостающих деталей и неиндексируемых контрактов/документов. Обычному агенту CodeGraph был запрещён. Точные инструкции сохранены: [prompt-with.txt](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/prompt-with.txt), [prompt-without.txt](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/prompt-without.txt). По журналам первый действительно использовал CodeGraph, второй — `rg` и чтение исходников, без CodeGraph. Оба прочитали обязательные Context/ADR.

| Метрика | Обычный агент | С CodeGraph | Изменение с CodeGraph |
| --- | ---: | ---: | ---: |
| Все обработанные токены: input + output | 592 070 | 633 591 | +7,0% |
| Input, включая кэш | 589 266 | 630 844 | +7,1% |
| Cached input, часть input | 544 384 | 575 360 | +5,7% |
| Input без кэша | 44 882 | 55 484 | +23,6% |
| Output, включая reasoning | 2 804 | 2 747 | −2,0% |
| Input без кэша + output | 47 686 | 58 231 | +22,1% |
| Время от task_started до task_complete | 82,624 с | 91,005 с | +10,1% |
| Запросы модели со счётчиками usage | 13 | 13 | одинаково |
| Вызовы native `exec` | 12 | 12 | одинаково |
| Оценка независимого рецензента | 23/24 | 24/24 | практически равное качество |

Токены взяты из накопительных `token_count.total_token_usage` отдельных свежих сессий. По каждому полю сумма 13 `last_token_usage` совпала с итоговым счётчиком; накопительные значения монотонны. Это сумма обработки контекста всеми запросами, включая повторно обработанный контекст и системные инструкции, а не число уникальных токенов или размер окна. Кэш уже входит в input, reasoning уже входит в output; отдельно их не прибавляли. Вызов `exec` мог включать несколько команд, поэтому 12 вызовов не означает 12 CLI-запросов. Измерение исключает оркестратора, рецензента и предварительную установку/индексацию. Метаданные, точные счётчики и воспроизводимый аудит: [metadata.json](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/metadata.json), [measured-results.json](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/measured-results.json), [comparison.json](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/comparison.json), [usage-audit.json](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/usage-audit.json), [audit_usage.py](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/audit_usage.py).

Свежий reviewer получил обезличенные ответы A/B, задачу и шкалу из шести требований. Ему не показывали методы получения кода, метрики, mapping и tool-логи. Только последние фразы о методе чтения были скрыты; содержательные ответы сохранены полностью. Шкала записана после получения ответов, до независимой оценки: это не заранее зарегистрированная методика. Рецензент признал практическое качество одинаковым и подтвердил все четыре теста, указанные обоими агентами. У обычного агента отмечено небольшое чрезмерное обобщение: известная потеря источника не всегда означает `partial`; при отсутствующем источнике и неизвестном `has_next` возможен `unknown`. Ответы и оценка: [answer-with.md](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/answer-with.md), [answer-without.md](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/answer-without.md), [quality-review.md](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/quality-review.md), [quality-scores.json](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/quality-scores.json), [rubric.json](../../.work/codegraph-evaluation/2026-10-01/ab-reviews/rubric.json).

Пределы сравнения: одна задача, один прогон на вариант, готовый индекс, общая машина и параллельное выполнение; случайная вариативность и конкуренция за ресурсы не исключены. Сравнивался CLI с обычными инструментами; доступного CodeGraph MCP tool namespace в этих агентах не было. CodeGraph-агент применял `node`/`explore`, включая первые неточные запросы, и адресный fallback. Это результат данного способа работы, а не отдельный тест MCP `codegraph_explore` с оптимально настроенными подсказками. Счётчики не переводились в деньги или лимит подписки. По этой паре прогонов оснований обещать экономию или делать CodeGraph обязательным инструментом проекта нет.

## Как использовать дальше

Из корня проекта:

```sh
codegraph explore "commit_search commit_operation prepare_reply"
codegraph callers validate_output --json
codegraph callees commit_search --json --limit 100
codegraph status
```

Рекомендация: оставить установленным и использовать для поиска нужных участков и возможных зависимостей. Перед решением о последствиях изменения сверять критические связи с исходниками; полный набор проектных checks по-прежнему определяет [verification](../behavior.md#verification). Результат `affected` для выбора Rust-тестов здесь непригоден.

Для каждого нового проекта или worktree нужен свой `codegraph init`. Worktree без собственного индекса может использовать индекс предка с предупреждением: [worktree.ts](https://github.com/colbymchenry/codegraph/blob/v1.6.1/src/sync/worktree.ts#L4-L17). Изменения вне активного MCP-сеанса можно согласовать командой `codegraph sync`.

Обещанные проценты экономии из README относятся к авторскому A/B на Claude Code и семи архитектурных вопросах; наша пара прогонов выше проверяет другую задачу и другой агентный интерфейс. Методика и отдельный рост остаточного контекста описаны самим проектом: [README](https://github.com/colbymchenry/codegraph/blob/v1.6.1/README.md#L196-L251).

Производственный код Ozon MCP не изменялся. В ходе оценки и A/B полный `scripts/check.py`, Chromium и live Ozon не запускались: проверялась установка, навигация и агентное исследование исходников. В этих агентных заданиях упомянутые Rust-тесты читались, но не выполнялись. Логи сохранены в `.work/codegraph-evaluation/2026-10-01/`, A/B — в подкаталоге `ab-reviews/`; эти локальные артефакты исключены из Git.

Перед публикацией настройки полный `scripts/check.py --offline` прошёл все шесть этапов за 15,422 с: Rust fmt 0,327 с, browser build 0,414 с, Node tests 0,424 с, contracts 0,264 с, Rust tests 13,645 с, Clippy 0,348 с. Rust: 164 passed, 1 ignored; Node: 22 passed. Использованы существующий Rust 1.95.0 из Nix store и закреплённый `jsonschema[format]` 4.25.1. Результаты и пути отдельных логов: [results.json](../../.work/checks/codegraph-main-20261001/gate-nix/results.json). Chromium и live Ozon в этот gate не входят.
