# Разработка

## Структура

| Крейт | Назначение |
| --- | --- |
| `crates/tracker-api` | Спецификация Tracker API, генератор таблицы операций (`build.rs`, `build/`), исполнитель HTTP-запросов, хранилище токенов, IAM-токены через `yc` (`src/iam.rs`). |
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

Тесты не обращаются к реальному Tracker API (HTTP подменяется `wiremock`), не используют системное хранилище токенов и не запускают настоящий `yc`: его подменяет скрипт во временном каталоге (тесты IAM — только на Unix), а время — управляемые часы.

Готовые сборки: macOS (universal), Linux x86_64 и aarch64 (статические, musl) и Windows x86_64 (`x86_64-pc-windows-msvc`).

### Windows

Сборка — нативная, целевая платформа `x86_64-pc-windows-msvc`: нужны Visual Studio Build Tools с компонентом «Разработка классических приложений на C++» и `rustup`. Релизный бинарник собирается со статическим CRT, чтобы на машине пользователя не требовался Visual C++ Redistributable:

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked --target x86_64-pc-windows-msvc -p tracker-mcp
cargo clippy --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc
```

Ветки кода для Windows (хранилище токенов в Credential Manager, запуск `claude.cmd`, сравнение файлов без inode в исполнителе) компилируются или выполняются только там, поэтому проверяйте изменения в них этими командами. С macOS и Linux `cargo check --target x86_64-pc-windows-msvc` без Windows SDK не проходит: C-код `aws-lc-sys` требует заголовков Windows.

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

Проверить, не отстала ли спецификация от справочника (новые и удалённые страницы, операции и параметры; код выхода `3`, если есть разница): `python3 crates/tracker-api/spec/build_openapi.py --check`.

Что внутри спецификации, какие расхождения справочника с реальным API исправлены и что не проверено — в [описании спецификации](../crates/tracker-api/spec/README.md).

При сборке `build.rs` превращает спецификацию в таблицу операций: имя инструмента, метод, путь, уровень доступа (`x-access-level`), набор (тег спецификации) и JSON Schema аргументов. Чтобы обновить инструменты:

1. Пересоберите спецификацию скриптом. JSON не правится руками: расхождения справочника с API исправляются исключениями в скрипте, чтобы не терялись при пересборке.
2. Соберите проект. Новый тег спецификации без slug набора — ошибка сборки: добавьте slug в `crates/tracker-api/build/toolsets.rs`.
3. Проверьте снапшоты операций (`crates/tracker-api/tests/snapshots/`) и уровни доступа новых операций; снапшоты обновляются осознанно: `cargo insta review` или `INSTA_UPDATE=always cargo test -p tracker-api`.
4. Обновите [MCP-инструменты](tools.md), если изменились наборы или число инструментов.

Сгенерированный код не правится вручную: единственный источник операций — спецификация.
