## Context

Крейт `crates/embedding` — пустой стаб (только doc-comment, зависимость `config`). Мотивация — proposal.md (Why). Текущее состояние: `crates/config` уже содержит полный ONNX-конфиг (`OnnxConfig`, `PlatformForKey`, `ModelForName`, `ArchiveFormat` — из change config-module); корневой `Cargo.toml` уже объявляет `indicatif = "0.18"` в палитре. Граф зависимостей (D1): `db, vectors → embedding`; embedding используется ингестией (change `ingestion`), запросный путь модель НЕ грузит.

Референсы оракула: `../synopsis/internal/onnx/{downloader.go, library.go, library_cache.go, library_registry.go, model-manager.go, model-cache.go, model-registry.go}`, `../synopsis/internal/embedding/{provider.go, onnx_provider.go, cache.go, tokenizer.go, sugarme_tokenizer.go, processor.go}`, `../synopsis/configs/onnx.yaml`.

## Goals / Non-Goals

**Goals:**
- Полный локальный пайплайн эмбеддингов: ONNX Runtime lifecycle (внешний `.so`/`.dylib`), менеджер моделей, загрузчик, токенизация, кэш, провайдер.
- Пин bindings-крейта (ADR 0002): `ort 2.0.0-rc.13` + `load-dynamic`.
- Машинно-проверяемый паритет без сети и без модели в CI.

**Non-Goals:**
- NER (change `ingestion`), APIProvider, персистентный кэш, GPU/EP, скачивание моделей в CI.

## Decisions

### D1. Bindings: `ort 2.0.0-rc.13` + `load-dynamic` (ADR 0002)
**Решение:** `ort = "2.0.0-rc.13"` с фичей `load-dynamic` (без `download-binaries`). ONNX Runtime 1.28 (внешний `.so`/`.dylib`/`.dll` по замороженному стеку). Загрузка библиотеки по явному пути через `ort::init_from(path)` — аналог Go `SetSharedLibraryPath` + `InitializeEnvironment`.
**Почему не альтернатива:** `download-binaries` (орт сам качает .so в target/) — нарушает frozen stack «внешний .so по onnx.yaml» и контроль версии; raw FFI через ort-sys — теряем безопасную обёртку; candle/tract — другой движок, ломает стек. ort — де-факто стандарт (16M загрузок, MIT/Apache-2.0, активная поддержка, rc.13 от 2026-07-28). Pre-release принят осознанно: API-чарн изолирован в `runtime.rs`.
**Референс:** `../synopsis/internal/embedding/onnx_provider.go` (SetSharedLibraryPath/InitializeEnvironment), `../synopsis/internal/onnx/library.go`.

### D2. Токенизатор: `tokenizers 0.23.1` (HF)
**Решение:** обёртка над HF `tokenizers` (загрузка `tokenizer.json`, который поставляет bge-m3; encode → ids + attention mask; truncation до max_length=512; decode).
**Почему не альтернатива:** sugarme (Go) не имеет Rust-аналога; свой токенизатор — изобретение WordPiece/BPE с спец-токенами, ошибкоопасно. tokenizers — стандарт HF-формата (27M загрузок, Apache-2.0).
**Референс:** `../synopsis/internal/embedding/sugarme_tokenizer.go`, `tokenizer.go`.

### D3. Кэш эмбеддингов: memory-only (YAGNI)
**Решение:** `HashMap<sha256(model|dim|text), Vec<f32>>` + `RwLock`, max_size=10000, при переполнении — очистка (поведение оракула). Персистентность — позже, через db/app_kv.
**Почему не альтернатива:** персистентный кэш через db добавляет зависимость и DAO-слой без текущей потребности (ингестия — однопроходная).
**Референс:** `../synopsis/internal/embedding/cache.go` (CacheKey = sha256(model|dim|text)).

### D4. Батч-инференс + `Arc<Mutex<Session>>`
**Решение:** все тексты батча — в один ONNX-run (улучшение над Go: там batch=1 последовательно). `Session::run(&mut self)` → `Arc<Mutex<Session>>` для потокобезопасности. Инференс — в `spawn_blocking` (sync-код, паттерн крейта db).
**Почему не альтернатива:** per-text инференс — медленнее; `spawn_blocking` на каждый текст — избыточно.
**Референс:** `../synopsis/internal/embedding/onnx_provider.go` (infer, batch=1).

### D5. Декомпозиция: 8 модулей
`lib.rs` (trait `EmbeddingProvider` + фабрика), `runtime.rs` (ort env + session builder), `library.rs` (LibraryManager: .so download/extract/cache), `downloader.rs` (HTTP), `model.rs` (ModelManager + .cache.json), `tokenizer.rs`, `cache.rs`, `provider.rs` (OnnxProvider). Каждый ≤500 строк.
**Почему не альтернатива:** меньше файлов — нарушение лимита задач; больше — избыточная фрагментация.
**Референс:** структура `internal/onnx` + `internal/embedding` оракула.

### D6. APIProvider НЕ портируется
**Решение:** OpenAI-совместимый HTTP-провайдер оракула не переносится — локальный сценарий, YAGNI.
**Почему не альтернатива:** +~200 строк без потребителя; при необходимости добавится отдельным change.
**Референс:** `../synopsis/internal/embedding/api_provider.go`.

### D7. Потоки сессии: intra=2, inter=1
**Решение:** `with_intra_threads(2)` + `with_inter_threads(1)` + `GraphOptimizationLevel::Level1`. bge-m3 int8 — малая модель; 16GB ноутбук, без перегрева CPU.
**Почему не альтернатива:** авто-детект CPU — рискованно на shared-ноутбуках; явная конфигурация безопаснее.
**Референс:** `../synopsis/internal/onnx/library.go` (session options).

### D8. Загрузчик: `ureq` + `indicatif` + SSRF + size-верификация
**Решение:** sync HTTP (`ureq`, уже в зависимостях ort), 3 ретрая × 2s, timeout 10m, SSRF-защита (resolve → reject private/loopback/link-local), прогресс через `indicatif` (уже в палитре), верификация по `size_bytes` из onnx.yaml после скачивания (улучшение: Go проверяет только существование файла).
**Почему не альтернатива:** reqwest — async-стек без нужды (загрузка не в hot path); без size-проверки — молчаливая порча модели.
**Референс:** `../synopsis/internal/onnx/downloader.go` (ретраи/SSRF/прогресс), `model-cache.go` (IsInstalled).

### D9. Отмена (ctx) — без аналога в v1
**Решение:** sync API без cancellation-токена. В sync-коде за `spawn_blocking` отмена не работает (токио не может прервать блокирующий поток); Ctrl+C обрабатывается на уровне рантайма.
**Почему не альтернатива:** CancellationToken добавил бы сложность без эффекта в sync-контексте; отклонение от оракула (context.Context) задокументировано.
**Референс:** `../synopsis/internal/embedding/onnx_provider.go` (ctx.Done() между текстами).

### D10. Симлинки для .so не нужны
**Решение:** `ort::init_from` грузит по явному пути из кэша; unversioned-симлинк (`libonnxruntime.so`) не создаётся.
**Почему не альтернатива:** Go создавал симлинк для `dlopen` по имени; ort принимает полный путь.
**Референс:** `../synopsis/internal/onnx/library.go`.

## Risks / Trade-offs

- [ort rc API-чарн до stable 2.0] → пин точной версии в Cargo.toml; изоляция ort-API в `runtime.rs` (единственный файл правок при миграции).
- [load-dynamic на windows-gnu / darwin-arm64] → проверка на CI-таргетах (cargo zigbuild); при сбое — fallback на системный путь поиска.
- [tokenizer.json bge-m3 vs tokenizers 0.23.1] → тест на фикстуре tokenizer.json; при несовместимости правки только в `tokenizer.rs`.
- [2.3GB model.onnx_data — UX скачивания] → прогресс-бар + предупреждение о размере до скачивания.
- [CI без .so и без сети] → все ort-зависимые тесты `#[ignore]`; юнит-тесты на mock-HTTP и фикстурах.
- [Session::run(&mut self) сериализует инференс] → приемлемо: ингестия однопоточная, батч-инференс компенсирует.

## Migration Plan

Новый крейт — обратной совместимости не требуется (внутренний уровень). Порядок: E1 (скаффолдинг) → E2/E3/E6/E7 (независимые модули) → E4/E5 (загрузчики поверх E3) → E8 (провайдер) → E9 (сборка). CI: `cargo test -p embedding` без сети; `#[ignore]`-тесты — вручную при наличии модели.

## Open Questions

- (нет — все решения приняты; deferrable-неизвестные: точное поведение ort load-dynamic на экзотических таргетах — проверяется в E2, не меняет спеки/задачи)