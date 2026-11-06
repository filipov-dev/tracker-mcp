//! Генерация таблицы операций из OpenAPI-спецификации Tracker API.
//!
//! Результат — `$OUT_DIR/operations.rs` со статическими таблицами `OPERATIONS` и
//! `TOOLSETS`, которые подключает `src/spec.rs`. Подробности:
//!
//! - `build/generate.rs` — разбор операций, параметров, тела и ответа, вывод кода;
//! - `build/naming.rs` — имена инструментов;
//! - `build/toolsets.rs` — slug наборов инструментов для тегов спецификации;
//! - `build/schema.rs` — JSON Schema аргументов с раскрытыми `$ref`.

#[path = "build/generate.rs"]
mod generate;
#[path = "build/naming.rs"]
mod naming;
#[path = "build/schema.rs"]
mod schema;
#[path = "build/toolsets.rs"]
mod toolsets;

use std::{env, fs, path::PathBuf};

const SPEC_PATH: &str = "spec/tracker.openapi.json";

fn main() {
    println!("cargo:rerun-if-changed={SPEC_PATH}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build");

    let raw = fs::read_to_string(SPEC_PATH).expect("не удалось прочитать спецификацию Tracker");
    let spec: serde_json::Value =
        serde_json::from_str(&raw).expect("спецификация Tracker не является корректным JSON");

    let code = generate::generate(&spec);

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR не задан")).join("operations.rs");
    fs::write(out, code).expect("не удалось записать сгенерированные операции");
}
