# Разработка

## Структура

| Крейт | Назначение |
| --- | --- |
| `crates/tracker-api` | Спецификация Tracker API, генератор таблицы операций (`build.rs`, `build/`), исполнитель HTTP-запросов, хранилище токенов. |
| `crates/tracker-mcp-core` | MCP-инструменты: реестр, уровни доступа, наборы, `api_request`, `api_search`, вырезание учётных данных, политика файлов. Не зависит от транспорта. |
| `crates/tracker-mcp` | Бинарник: CLI (`serve`, `connect`, `auth`) и транспорты stdio и HTTP (`src/cli/serve/http_server.rs`). |

## Сборка и проверки

Нужен Rust 1.88 или новее; `rust-toolchain.toml` выбирает стабильный канал с `rustfmt` и `clippy`. Для криптографии TLS нужен C-компилятор; системные библиотеки D-Bus и OpenSSL не нужны.

```bash
cargo build --release
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Тесты не обращаются к реальному Tracker API (HTTP подменяется `wiremock`) и не используют системное хранилище токенов.

Запустить CLI из исходников:

```bash
cargo run -p tracker-mcp -- --help
cargo run -p tracker-mcp -- serve --help
```

## Спецификация и генерация инструментов

Официальной OpenAPI-спецификации у Tracker нет. `crates/tracker-api/spec/tracker.openapi.json` собирается скриптом `build_openapi.py` из страниц [справочника Tracker API](https://yandex.ru/support/tracker/ru/api/about-api) (Python 3.10+, без сторонних пакетов):

```bash
python3 crates/tracker-api/spec/build_openapi.py --cache /tmp/tracker-api-docs
```

Что внутри спецификации, какие расхождения справочника с реальным API исправлены и что не проверено — в [описании спецификации](../crates/tracker-api/spec/README.md).

При сборке `build.rs` превращает спецификацию в таблицу операций: имя инструмента, метод, путь, уровень доступа (`x-access-level`), набор (тег спецификации) и JSON Schema аргументов. Чтобы обновить инструменты:

1. Пересоберите спецификацию скриптом. JSON не правится руками: расхождения справочника с API исправляются исключениями в скрипте, чтобы не терялись при пересборке.
2. Соберите проект. Новый тег спецификации без slug набора — ошибка сборки: добавьте slug в `crates/tracker-api/build/toolsets.rs`.
3. Проверьте снапшоты операций (`crates/tracker-api/tests/snapshots/`) и уровни доступа новых операций; снапшоты обновляются осознанно: `cargo insta review` или `INSTA_UPDATE=always cargo test -p tracker-api`.
4. Обновите [MCP-инструменты](tools.md), если изменились наборы или число инструментов.

Сгенерированный код не правится вручную: единственный источник операций — спецификация.
