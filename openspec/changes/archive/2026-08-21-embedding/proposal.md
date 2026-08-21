## Why

Крейт `crates/embedding` — пустой стаб. Для ингестии (change `ingestion`) нужен локальный пайплайн эмбеддингов: ONNX Runtime как внешний `.so`/`.dylib` (замороженный стек), модель bge-m3 int8 (1024-dim), токенизация, кэш. Биндинги-крейт был отложен (onnxruntime-rs исчез с crates.io, ort без стабильного релиза) — финальный пин делается в этом change (ADR 0002).

## What Changes

- Новый крейт `crates/embedding`: ONNX Runtime lifecycle (скачивание/распаковка `.so` по onnx.yaml, кэш), менеджер моделей (registry + скачивание + кэш), HTTP-загрузчик (ретраи, SSRF-защита, прогресс), токенизатор (HF `tokenizers`, tokenizer.json), кэш эмбеддингов (in-memory), провайдер `OnnxProvider` (батч-инференс, L2-нормализация).
- Пин bindings: **ort 2.0.0-rc.13** с фичей `load-dynamic` (внешний `.so` по явному пути — аналог Go `SetSharedLibraryPath`; QDQ-фьюжн для int8 включён по умолчанию). ADR 0002.
- Токенизатор: **tokenizers 0.23.1** (HF-формат tokenizer.json, который поставляет bge-m3).
- Загрузчик: **ureq** (sync, уже в зависимостях ort) + **indicatif** (прогресс-бар, уже в палитре).
- **BREAKING (внутреннее, не контрактное):** APIProvider (OpenAI-совместимый HTTP-провайдер оракула) НЕ портируется — локальный сценарий, YAGNI. Кэш эмбеддингов — memory-only (персистентность позже, через db). Отмена (context.Context) — без аналога в v1 (sync-код за spawn_blocking).
- NER — вне scope (живёт в `internal/ingestion/ner/` оракула, переедет в change `ingestion`).

## Capabilities

### New Capabilities
- `embedding`: локальный пайплайн эмбеддингов — lifecycle ONNX Runtime (внешний `.so`), менеджер моделей (скачивание/кэш по onnx.yaml), токенизация, кэш эмбеддингов, провайдер с батч-инференсом и L2-нормализацией.

### Modified Capabilities
- (нет — контрактные спеки mcp-contract/cli-surface/data-schema/config-format не затрагиваются; onnx.yaml уже описан в config-format)

## Impact

- `crates/embedding/*` — новый код (8 модулей, ~1100 строк с тестами).
- Корневой `Cargo.toml` — палитра: `ort = "2.0.0-rc.13"` (features: load-dynamic), `tokenizers = "0.23.1"`, `ureq`, `sha2`, `zip`, `flate2`, `tar`, `serde_json` (часть уже есть).
- Зависимости крейта: `config` (OnnxConfig из config-module), `db` (по графу D1; в v1 не используется — кэш memory-only).
- Замороженные контракты: не затрагиваются. Паритет: конфиг-parity (OnnxConfig), загрузчик на mock-HTTP, семантика кэша, токенизатор на фикстуре tokenizer.json, L2-нормализация на golden-векторах; реальный инференс — за `#[ignore]` (CI без сети и без модели).

## Non-goals

- NER (change `ingestion`).
- APIProvider (OpenAI-совместимый HTTP) — не портируется.
- Персистентный кэш эмбеддингов (позже, через db/app_kv).
- Векторный поиск/индекс (change `vectors`).
- Скачивание моделей в CI; реальный инференс в CI.
- Поддержка GPU/CUDA/прочих execution providers (CPU-only, ноутбук).