# Tasks — embedding

Порядок = граф зависимостей: 1.1 → {1.2, 1.3, 1.6, 1.7} → {1.4, 1.5} → 1.8 → 1.9. Каждая задача выполняется СВЕЖИМ агентом без памяти предыдущих — тело самодостаточно. Формат: чекбокс + блок деталей (цель / scope / зависимости / критерии приёмки / референс / история ревизий). **Принцип миграции (binding, повторяется в каждой задаче):** НЕ транскрибировать Go 1:1 — функциональная копия, не кодовая; архитектурно правильно для Rust (DRY, KISS, SOLID, YAGNI); внутренняя совместимость с оракулом не требуется; баги Go исправлять или фиксировать осознанные отклонения. CI без сети: все тесты, требующие реального ONNX Runtime/модели — `#[ignore]`.

## 1. Реализация

- [x] 1.1 Скаффолдинг крейта embedding: Cargo.toml, trait EmbeddingProvider, ошибки
  - **Цель:** подготовить крейт к реализации: зависимости (ort 2.0.0-rc.13 с фичей load-dynamic, tokenizers 0.23.1, ureq, sha2, zip, flate2, tar, serde_json, indicatif — из workspace-палитры, добавить недостающие в корневой Cargo.toml), публичный trait `EmbeddingProvider` (generate_embeddings/vector_dim/name), типы ошибок `EmbeddingError` (thiserror: Ort, Tokenizer, Download, Cache, Model, Io, Config). **НЕ транскрибировать Go 1:1** (принцип миграции).
  - **Scope файлов:** `crates/embedding/Cargo.toml`, корневой `Cargo.toml` (палитра), `crates/embedding/src/lib.rs` (trait + re-exports + crate docs), `crates/embedding/src/error.rs` (новый). Модули-заглушки НЕ создавать (только то, что в scope).
  - **Зависимости:** нет (config-крейт уже готов из config-module).
  - **Критерии приёмки:** `cargo check -p embedding` и `cargo test -p embedding` зелёные; trait `EmbeddingProvider: Send + Sync` с методами `generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError>`, `vector_dim(&self) -> usize`, `name(&self) -> &'static str`; `EmbeddingError` с вариантами и `Display`/`std::error::Error`; missing_docs=deny соблюдён; fmt/clippy чисто; workspace зелёный.
  - **Референс:** `../synopsis/internal/embedding/provider.go` (интерфейс Provider), `../synopsis/internal/embedding/onnx_provider.go` (Name/VectorDim).
  - **История ревизий:**
    - Ревизия 1 (2026-08-20): первая версия (решения человека 2026-08-20: ort rc.13 load-dynamic, tokenizers 0.23.1, ureq, memory-only кэш, без APIProvider, без cancellation).

- [x] 1.2 Модуль runtime: ort-окружение и SessionBuilder
  - **Цель:** изолировать весь ort-API в одном модуле: `init_runtime(lib_path)` — `ort::init_from(path)` (загрузка внешнего .so/.dylib по явному пути, D1/D10), `build_session(model_path)` — `Session::builder()` с `with_intra_threads(2)`, `with_inter_threads(1)`, `GraphOptimizationLevel::Level1` (D7), `commit_from_file`. QDQ-фьюжн для int8 — по умолчанию. **НЕ транскрибировать Go 1:1** (принцип миграции).
  - **Scope файлов:** `crates/embedding/src/runtime.rs` (новый) + регистрация в lib.rs.
  - **Зависимости:** 1.1 (ошибки, Cargo.toml).
  - **Критерии приёмки:** `cargo test -p embedding` зелёный; юнит-тесты: `init_runtime` с несуществующим путём возвращает `EmbeddingError::Ort` (без паники); `build_session` с несуществующим файлом модели — ошибка; тесты с реальным .so/моделью — `#[ignore]`; fmt/clippy чисто.
  - **Референс:** `../synopsis/internal/embedding/onnx_provider.go` (SetSharedLibraryPath/InitializeEnvironment/NewDynamicAdvancedSession), `../synopsis/internal/onnx/library.go` (session options).
  - **История ревизий:**
    - Ревизия 1 (2026-08-20): первая версия.
    - Ревизия 2 (2026-08-20): отклонение, зафиксировано реализатором + ревьюером: `error.rs` (вне scope) — вариант `Ort(#[from] ort::Error)` заменён на `Ort(String)` + ручные `From<ort::Error>`/`From<ort::LoadDynamicError>`. Причина: в ort 2.0.0-rc.13 ЛЮБОЙ конструктор ort::Error вызывает C API (CreateStatus), а в load-dynamic без загруженного .so это паника — вариант из 1.1 принципиально не мог представить сбой «библиотека не загружена». Риппла нет (внутренний тип, ноль использований вне error.rs). Минор (на будущее): `Ort(String)` теряет error source chain; при переходе на ort stable 2.0.0 — пересмотреть.

- [x] 1.3 Модуль downloader: HTTP-загрузка с ретраями, SSRF-защитой, прогрессом
  - **Цель:** sync-загрузчик (ureq): 3 ретрая × 2s, timeout 10m, User-Agent, SSRF-защита (resolve hostname → reject private/loopback/link-local), прогресс через indicatif, верификация размера против ожидаемого (D8; улучшение над Go — там только существование). Частичный файл при ошибке удаляется. **НЕ транскрибировать Go 1:1** (принцип миграции).
  - **Scope файлов:** `crates/embedding/src/downloader.rs` (новый) + регистрация в lib.rs.
  - **Зависимости:** 1.1 (ошибки).
  - **Критерии приёмки:** `cargo test -p embedding` зелёный; тесты на локальном mock-HTTP-сервере (std TcpListener в тесте): успешное скачивание + размер; временная ошибка → ретрай → успех; исчерпание попыток → ошибка + частичный файл удалён; SSRF: URL на 127.0.0.1/10.x/192.168.x/172.16-31.x отклоняется ДО запроса; несовпадение размера → ошибка; fmt/clippy чисто.
  - **Референс:** `../synopsis/internal/onnx/downloader.go` (ретраи/SSRF/прогресс/checksum), `downloader_test.go`.
  - **История ревизий:**
    - Ревизия 1 (2026-08-20): первая версия.

- [ ] 1.4 Модуль library: LibraryManager — скачивание/распаковка ONNX Runtime .so
  - **Цель:** обеспечение внешней библиотеки ONNX Runtime: по `OnnxConfig.runtime` (PlatformForKey для текущей ОС/архитектуры) скачать архив (zip/tgz), распаковать, извлечь библиотеку по `library_path`, сохранить `.cache.json` (версия, путь, платформа, время установки). Повторный вызов при совпадении версии — без скачивания. Ошибка → явная ошибка, частичные файлы не помечаются установленными. **НЕ транскрибировать Go 1:1** (принцип миграции).
  - **Scope файлов:** `crates/embedding/src/library.rs` (новый) + регистрация в lib.rs.
  - **Зависимости:** 1.1 (ошибки), 1.3 (downloader).
  - **Критерии приёмки:** `cargo test -p embedding` зелёный; тесты на mock-HTTP с локальными zip/tgz-фикстурами: установка с нуля (скачивание+распаковка+кэш); повторный вызов без скачивания; несовпадение версии → переустановка; ошибка скачивания → ошибка, кэш не помечен; fmt/clippy чисто.
  - **Референс:** `../synopsis/internal/onnx/library.go`, `library_cache.go`, `library_registry.go`, `../synopsis/configs/onnx.yaml` (runtime-секция).
  - **История ревизий:**
    - Ревизия 1 (2026-08-20): первая версия.

- [ ] 1.5 Модуль model: ModelManager + ModelCache
  - **Цель:** менеджер моделей: реестр из `OnnxConfig.models` (ModelForName, default), `ensure_model(name)` — установлена (кэш + файлы есть) → путь; нет → скачать все файлы (url/size из onnx.yaml) через downloader, пометить установленной в `.cache.json`; неизвестное имя → ошибка. Верификация размера после скачивания (D8). **НЕ транскрибировать Go 1:1** (принцип миграции).
  - **Scope файлов:** `crates/embedding/src/model.rs` (новый) + регистрация в lib.rs.
  - **Зависимости:** 1.1 (ошибки), 1.3 (downloader).
  - **Критерии приёмки:** `cargo test -p embedding` зелёный; тесты на mock-HTTP с локальными файлами: ensure_model установленной модели → путь без скачивания; неустановленной → скачивание всех файлов + кэш; неизвестное имя → ошибка; default при пустом имени; несовпадение размера → ошибка, модель не помечена; fmt/clippy чисто.
  - **Референс:** `../synopsis/internal/onnx/model-manager.go`, `model-cache.go`, `model-registry.go`, `../synopsis/configs/onnx.yaml` (models-секция).
  - **История ревизий:**
    - Ревизия 1 (2026-08-20): первая версия.

- [x] 1.6 Модуль tokenizer: обёртка над HF tokenizers
  - **Цель:** обёртка над `tokenizers::Tokenizer`: загрузка из tokenizer.json (путь к файлу), `tokenize(text) -> Vec<u32>` с truncation до max_length=512, `decode(ids) -> String`. **НЕ транскрибировать Go 1:1** (принцип миграции).
  - **Scope файлов:** `crates/embedding/src/tokenizer.rs` (новый) + регистрация в lib.rs.
  - **Зависимости:** 1.1 (ошибки).
  - **Критерии приёмки:** `cargo test -p embedding` зелёный; тест на фикстуре tokenizer.json (мини-фикстура в tests/fixtures или сгенерированная в тесте): encode → ids + attention mask; truncation на границе max_length; decode round-trip; отсутствующий файл → ошибка; fmt/clippy чисто.
  - **Референс:** `../synopsis/internal/embedding/sugarme_tokenizer.go`, `tokenizer.go` (DefaultMaxLength).
  - **История ревизий:**
    - Ревизия 1 (2026-08-20): первая версия.

- [ ] 1.7 Модуль cache: EmbeddingCache (memory-only)
  - **Цель:** in-memory кэш эмбеддингов: `HashMap<sha256(model|dim|text), Vec<f32>>` + `RwLock`, max_size=10000 (конструктор с параметром), при переполнении — очистка (поведение оракула, D3). **НЕ транскрибировать Go 1:1** (принцип миграции).
  - **Scope файлов:** `crates/embedding/src/cache.rs` (новый) + регистрация в lib.rs.
  - **Зависимости:** 1.1 (ошибки).
  - **Критерии приёмки:** `cargo test -p embedding` зелёный; тесты: set/get round-trip; ключ зависит от (модель, dim, текст) — разные тексты/модели не пересекаются; eviction при max_size; конкурентный доступ (несколько потоков); fmt/clippy чисто.
  - **Референс:** `../synopsis/internal/embedding/cache.go` (CacheKey, maxSize).
  - **История ревизий:**
    - Ревизия 1 (2026-08-20): первая версия.

- [ ] 1.8 Модуль provider: OnnxProvider — батч-инференс, L2-нормализация, кэш
  - **Цель:** реализация `EmbeddingProvider`: `OnnxProvider { session: Arc<Mutex<Session>>, tokenizer, cache, vector_dim, model_name }`. `generate_embeddings`: пустой список → ошибка; per-text кэш-проверка; токенизация; батч-инференс одним ONNX-run (D4); L2-нормализация каждого вектора (norm≈0 → без деления); сохранение в кэш. Инференс — sync (вызывающий сам решает про spawn_blocking). **НЕ транскрибировать Go 1:1** (принцип миграции).
  - **Scope файлов:** `crates/embedding/src/provider.rs` (новый) + регистрация в lib.rs.
  - **Зависимости:** 1.1 (trait/ошибки), 1.2 (runtime), 1.6 (tokenizer), 1.7 (cache).
  - **Критерии приёмки:** `cargo test -p embedding` зелёный; юнит-тесты без реальной модели: пустой список → ошибка; кэш-интеграция (мок-инференс через тестовый хук или подмену сессии — если невозможно без ort, тестировать через `#[cfg(test)]`-фабрику); L2-нормализация на golden-векторах (известный вход → известный нормализованный выход); реальный инференс — `#[ignore]`; fmt/clippy чисто.
  - **Референс:** `../synopsis/internal/embedding/onnx_provider.go` (GenerateEmbeddings/infer/L2), `provider_test.go`.
  - **История ревизий:**
    - Ревизия 1 (2026-08-20): первая версия.

- [ ] 1.9 Сборка: фабрика new_onnx_provider + финальные гейты
  - **Цель:** фабрика `new_onnx_provider(cfg, data_dir, onnx_cfg) -> Result<Arc<dyn EmbeddingProvider>, EmbeddingError>`: LibraryManager.ensure_library → ModelManager.ensure_model → tokenizer → runtime.build_session → OnnxProvider. Re-exports всех публичных типов в lib.rs. Полный прогон гейтов. **НЕ транскрибировать Go 1:1** (принцип миграции).
  - **Scope файлов:** `crates/embedding/src/lib.rs` (фабрика + re-exports + crate docs), финальная проверка: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`, `cargo doc -p embedding`.
  - **Зависимости:** 1.2, 1.4, 1.5, 1.8.
  - **Критерии приёмки:** `cargo test -p embedding` и `cargo test --workspace` зелёные; фабрика собирает провайдер из конфига (юнит-тест с мок-путями: отсутствующий .so/модель → ошибка с понятным сообщением; успешный путь — `#[ignore]`); `cargo doc -p embedding` без ошибок (missing_docs=deny); grep-проверка: в крейте нет ссылок на vec0; fmt/clippy чисто.
  - **Референс:** `../synopsis/internal/embedding/onnx_provider.go` (NewONNXProvider), `../synopsis/internal/onnx/model-manager.go` (EnsureModel).
  - **История ревизий:**
    - Ревизия 1 (2026-08-20): первая версия.